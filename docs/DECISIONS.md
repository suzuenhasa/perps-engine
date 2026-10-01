# Decision log

Every non-trivial choice, written when it was made (INFO.md section 2, rule 1). Big calls
that shape the architecture or the headline numbers were made by me after seeing options;
smaller ones were made by Claude Code and are logged here so I can defend them.

## Template

```
## D-NNN Title  (milestone, date, who decided)
**Decision:** one line.
**Context:** the constraint or number that forced a choice.
**Options considered:** at least two, and why each rejected one lost.
**Choice and evidence:** a benchmark or ablation row in docs/BENCHMARKS.md, a test, or a cited source.
**Trade-offs:** what we gave up.
**What would change it:** the condition under which we'd choose differently.
```

## Index

| ID | Title | Milestone | Status |
|---|---|---|---|
| D-001 | Build environment: Docker, read-only source, no network for builds | M0 | decided (me) |
| D-002 | Liquidation by insurance-fund takeover at the bankruptcy price | M2 | decided (me); implemented in M2 |
| D-003 | Liquidation-price index | M2 | decided (me); implemented in M2 |
| D-004 | Units: ticks, lots, micro-dollars; tick × lot = 1 micro | M0 | decided |
| D-005 | Fixed-size command and event records | M0 | decided |
| D-006 | Dependency policy: few, pinned, old enough, reviewed | M0 | decided |
| D-007 | Polymarket recorder and history puller design | M0 | decided |
| D-008 | Order-book semantics, defined by the reference book | M0, M1 | decided; modify rules confirmed (me) in M1 |
| D-009 | Property-test configuration | M0, M1 | decided; widened in M1 |
| D-010 | Fast order book: tick array, level bitmap, slab, intrusive lists | M1 | decided |
| D-011 | Seeded hash for client-chosen order ids | M1 | decided |
| D-012 | Book-layer synthetic flows (assumed parameters) | M1 | decided |
| D-013 | Isolated collateral model, release rule and rounding | M2 | decided (me: model; margin-call top-up, RISK.md Q1); spec in docs/RISK.md |
| D-014 | Leverage default and leverage tiers | M2 | decided (me: 1x default and capping); atomic tier tables after review |
| D-015 | Price band: derived bound, tighter than Polymarket; stale orders swept | M2 | decided (me: band rule; sweep, RISK.md Q3) |
| D-016 | Engine command semantics: validation order, event order | M2 | decided; revised after review |
| D-017 | O(1) worst-case check and the naive reference modes | M2 | decided; revised after review |
| D-018 | Liquidation keys: exact closed form; one ordered set per side | M2 | decided; extends D-003 |
| D-019 | Insurance-fund account: identity, netting, shortfall reporting | M2 | decided (me: fund doesn't trade in v1, RISK.md Q2); extends D-002 |
| D-020 | Size and price limits against overflow | M2 | decided; extends D-004; bounds tightened after review |
| D-021 | Signed messages: secp256k1 ECDSA via k256, SHA-256, fixed 136-byte format with an expiry | M3 | decided (me: k256; owner 2026-09-30: every signed message carries an expiry, and signatures and expiry are journaled, PIPELINE.md Q3, Q4); built in M3; evidence: local validation 2026-09-30; headline pending PERPSBOX |
| D-022 | Nonces: per account, strictly increasing, bounded jumps, used up at the account's gateway | M3 | built in M3; evidence: local validation 2026-09-30; headline pending PERPSBOX |
| D-023 | Canonical binary encoding of commands and events | M3 | built in M3; extends D-005; evidence: local validation 2026-09-30; headline pending PERPSBOX |
| D-024 | Rings: our own SPSC ring of atomic words; reject at the edge, wait inside, stop in data-flow order | M3 | decided (me: no queue crate); built in M3; evidence: local validation 2026-09-30; headline pending PERPSBOX (capacities against the probed fsync) |
| D-025 | Journal: segment files, CRC32C per record, group commit, output gating | M3 | decided (me: output gating; owner 2026-09-30: probe the disk first, and use a box with local NVMe for the durable rows if the flush p99 is over 5 ms; signatures and expiry journaled, PIPELINE.md Q2, Q3); built in M3; evidence: local validation 2026-09-30; headline pending PERPSBOX (the `T` sweep) |
| D-026 | Thread layout and pinning | M3 | decided (me: libc for pinning); built in M3; evidence: local validation 2026-09-30; headline pending PERPSBOX (the gateway count from the probes) |
| D-027 | M3 synthetic flow and open-loop load generator (assumed parameters) | M3 | built in M3; extends D-012; evidence: local validation 2026-09-30; headline pending PERPSBOX |
| D-028 | Measurement: stamps in records, our own histogram, percentiles over offered load, max rate at a limit | M3 | decided (me: own histogram; owner 2026-09-30: durable limit = 2 × T + the disk's p99 flush, PIPELINE.md Q1); built in M3; limit label and report revised after the build's review; evidence: local validation 2026-09-30; headline pending PERPSBOX |
| D-029 | Replay and the determinism test | M3 | built in M3; evidence: local validation 2026-09-30; headline pending PERPSBOX |
| D-030 | Crash recovery and restart | M3 | built in M3; recovery and fresh start revised after the build's review; evidence: local validation 2026-09-30; headline pending PERPSBOX |
| D-031 | Security boundary of v1, and what a network gateway needs first | M3 | spec (after the security review); built in M3; evidence: local validation 2026-09-30; headline pending PERPSBOX |
| D-032 | Optional libsecp256k1 verifier | M3 | decided (owner 2026-09-30: opt-in, measured side by side with `k256`); built in M3 after its supply-chain review; evidence: local 2026-09-30; headline pending PERPSBOX (the runbook's second pass) |
| D-033 | A second signing scheme matching Polymarket Perps: EIP-712 over keccak-256 of MessagePack, salt and timestamp, signer recovery | M3 add-on | decided (owner 2026-09-30: the scheme, opt-in, and our own keccak-256); built in M3; evidence: local 2026-09-30 (all 12 golden vectors, tests, probes); headline pending PERPSBOX (the runbook's EIP-712 passes) |
| D-034 | A Polymarket-shaped flow, calibrated from recorded data, and three stress switches | M3 add-on | decided (owner 2026-09-30: timing and scope); built in M3, with readings for the owner to confirm (the scale, the shock defaults); evidence: local 2026-09-30 (the calibration's `--check`s identical, tests, the realised shape, local runs); headline pending PERPSBOX (the runbook's D-034 passes, 7b) |

---

## D-001 Build environment: Docker, read-only source, no network for builds  (M0, 2026-09-29, me)

**Decision:** Everything that compiles or runs dependency code runs in a Docker container on
the local machine. Builds and tests see the source read-only, with `.git` hidden, and have
no network. Downloads happen in a separate container that compiles nothing.

**Context:** Crypto-adjacent packages are a common supply-chain target: in 2025 crates.io
removed crates that searched developers' files for crypto private keys. Cargo runs
dependency build scripts and procedural macros at build time, so even `cargo check` runs
third-party code. On this machine (WSL2), that code could read `~/.ssh` (GitHub and vast.ai
keys) and the whole Windows drive under `/mnt/c`. The project also has to be runnable by
other people.

**Options considered:**
- *Plain local build with supply-chain checks.* Simplest, but a hijacked crate runs on the
  first build, before any advisory exists. Rejected.
- *A rented remote box for everything.* Strongest isolation, but it costs money, the box can
  be recycled, and every edit needs a sync. Rejected for day-to-day work; still used for
  the headline benchmark runs (INFO.md 5a).
- *Local Docker (chosen).* Isolation from host secrets, and the Dockerfile doubles as the
  "how to run it" instructions for anyone else.

**Choice and evidence:** Container roles, all defined in `compose.yaml` and reached through
`./dev`. The guarantees come from these mounts and network settings, not from `./dev`,
whose allowlists only catch mistakes.

| Role | Source | Network | Cargo cache | Build output | Used for |
|---|---|---|---|---|---|
| `dev` | read-only | none | read-only | `target` | build, test, bench, clippy |
| `recorder-build` | read-only | none | read-only | `recorder-target` | builds only the recorder |
| `fmt` | read-write | none | none | none | `cargo fmt` (rustfmt only parses) |
| `fetch` | read-only | yes | read-write | none | `cargo fetch` |
| `lock` | read-only, except `Cargo.lock` | yes | read-write | none | `cargo generate-lockfile/update` |
| `check` | read-only | yes | read-only | none (advisory databases in `advisories`) | `cargo audit/deny/tree/metadata`, supply-chain scripts |
| `vet` | read-only, except `supply-chain/` | yes | read-only | none | `cargo vet` |
| `recorder` | none | yes | none | `recorder-target`, read-only | the WebSocket recorder; writes only `./data` |
| `history` | its own script, read-only | yes | none | none | the REST history puller; writes only `./data` |

In every container that mounts the source, `.git` is hidden behind an empty tmpfs. No
container sees `~/.ssh`, `/mnt/c`, the Docker socket or host environment variables.
Containers run as the host user, so files they write belong to me, not root.

`./dev --net` and `./dev --write` pick among `fetch`, `lock`, `check` and `vet` by command.

What this gives:
- Our dependencies' build scripts and proc-macros run only in `dev` and `recorder-build`,
  which have no network and can't write the source, `.git` or the cargo cache.
- Only cargo itself writes the cargo cache (`fetch`, `lock`). The third-party tools
  (cargo-audit, cargo-deny, cargo-vet) get it read-only (review finding, 2026-09-29).
  Otherwise they could edit unpacked sources, which cargo doesn't re-hash, or plant a
  `config.toml` with a linker or rustflags setting, and so shape what `dev` and
  `recorder-build` compile, the recorder included. After `cargo vet`, `./dev` also refuses
  to continue if `supply-chain/` holds anything but its three files (a planted
  `.git/config` would be read by git on the host).
- The cargo cache is read-only there (review finding, 2026-09-29). Otherwise code run
  during a build could plant a fake `cargo-<subcommand>` binary or a registry redirect
  in the cache for a networked container to pick up later. `HOME` is a throwaway `/tmp`,
  so no dotfiles persist between containers either.
- The recorder, which has network, runs a binary built in its own volume by
  `recorder-build`. So code run by other builds (tests, benchmarks) can't replace it.

Verified 2026-09-29, inside `dev`:
- Writing to `/work` fails (read-only file system).
- Writing to `/cargo` fails.
- `/work/.git` is empty.
- Outbound HTTPS fails.

Verified in the lockfile role (then called `write`): only `Cargo.lock` and
`supply-chain/` were writable, and `Cargo.lock` round-trips through `cargo update`
byte-for-byte. Verified after the split: `cargo audit`, `cargo deny check`, `cargo vet` and
both supply-chain scripts pass with the cache read-only.

**Host guard.** `.cargo/config.toml` replaces `rustc` with `tools/host-guard/rustc` and
also sets a `rustc-wrapper` (`tools/host-guard/rustc-wrapper`). Both refuse to run unless
`PERPS_IN_CONTAINER=1`, which only the image and the native benchmark box set. Cargo sends
even the compiler probe at the start of `cargo metadata` through them. So a stray
`cargo build` in this folder, or an editor's Rust support loading it, fails before any
dependency code runs. Checked on the host: `cargo metadata` fails at `rustc -vV` with the
guard's message, also with `RUSTC_WRAPPER` set to empty or to another program.

Limits: the config only applies to cargo started inside this folder (not, say,
`cargo build --manifest-path perpstest/Cargo.toml` from the parent), and the environment
beats it. `RUSTC_WRAPPER` (sccache, rust-analyzer's build-script pass) only replaces the
wrapper, which is why `rustc` itself is replaced too; setting `RUSTC` as well gets past
both. The VS Code rust-analyzer settings (`.vscode/settings.json`) remain as a second
layer for this editor.

Supply-chain checks on top (D-006): exact version pins, a two-week upgrade delay, reading
all build-time code before the first build, and `cargo audit`, `cargo deny` and
`cargo vet`. Results are in `docs/SUPPLY-CHAIN.md`.

**Trade-offs:** No editor autocomplete on the host (Dev Containers could restore it).
The image is ~1 GB. Local benchmarks run in Docker on WSL2, which is recorded with every
number. vast.ai boxes are containers themselves, so on a rented benchmark box we build
natively, with the same pinned toolchain, `--locked` dependencies and
`PERPS_IN_CONTAINER=1`; that box holds no secrets.

What still runs with network:
- The recorder's own dependencies (tokio, rustls, tungstenite), at run time. It is a
  network client, so this is unavoidable. Its container holds nothing but public data.
- The three supply-chain tools. Their dependency trees (several hundred crates) are
  compiled during `docker build` with network, and were not reviewed. They are trusted
  as well-known projects of RustSec, Embark and Mozilla, and confined as above. See
  `docs/SUPPLY-CHAIN.md`, "Not covered".

**What would change it:** A dependency that needs network access at build time (we'd drop
the dependency, not relax the rule). A need for editor support (add a Dev Containers
config that reuses this image). Distrust of the supply-chain tools (install their
checksum-pinned release binaries instead, or build them offline from pre-fetched
sources).

---

## D-002 Liquidation by insurance-fund takeover at the bankruptcy price  (M2, 2026-09-29, me)

**Decision:** In v1, when a position's equity falls below maintenance margin, the insurance
fund takes over the whole position (with its cost basis and locked collateral), which is
economically a takeover at the bankruptcy price. No liquidation orders go to the book, and
there is no ADL.

**Context:** v1 scope cut (INFO.md section 4). Polymarket's published waterfall is: close on
the book, then the insurance fund, then ADL
(https://docs.polymarket.com/perps/learn-about-trading/liquidation-mechanics.md). The full
waterfall is deferred to INFO.md section 12.2.

**Options considered:**
- *Book first, then fund.* More realistic, and exercises liquidations under load, but brings
  more code and edge cases (re-checking margin between fills, what the engine may send to
  the book). Deferred.
- *Fund takeover (chosen).* Fewest edge cases, fully deterministic.

**Choice and evidence:** Specified in INFO.md section 4 ("Liquidation (v1)", "The insurance
fund"). Evidence (unit tests, invariants, shortfall accounting) is added in Milestone 2.

**Trade-offs:** Departs from Polymarket's waterfall. The fund passively holds what it
absorbs, and its losses can exceed its capital; v1 reports that as uncovered shortfall
rather than falling back to ADL.

**What would change it:** Doing the section 12.2 work, or needing liquidation fills in the
load test.

---

## D-003 Liquidation-price index  (M2, 2026-09-29, me)

**Decision:** Keep positions per market in ordered structures keyed by exact liquidation
tick (plus account id for deterministic ties), so a mark update touches only the positions
it crosses.

**Context:** Rescanning every position on every mark update is one of the hot-path
anti-patterns in INFO.md section 8, and its cost grows with the number of accounts.

**Options considered:**
- *Scan all positions on each mark update.* Simplest; cost O(positions) per update, which
  stalls the core thread as accounts grow. Rejected.
- *Liquidation-price index (chosen).* About 150 lines; cost O(log n) per update plus the
  positions actually liquidated.

**Choice and evidence:** Key definition and walk rules in INFO.md section 4 ("Liquidation
index"). Evidence: the Milestone 2 microbenchmark of positions touched and time per mark
update, index vs full rescan, in `docs/BENCHMARKS.md`.

**Trade-offs:** Every position change pays an O(log n) re-key; the index must stay exactly
consistent with each slot (an invariant checked in tests).

**What would change it:** Measured re-key cost dominating the core's budget, e.g. with very
many positions per market.

---

## D-004 Units: ticks, lots, micro-dollars; tick × lot = 1 micro  (M0, 2026-09-29)

**Decision:** Prices are `i64` ticks, quantities `i64` lots, collateral `i64` micro-dollars.
Because one tick times one lot is exactly one micro-dollar, a fill's notional is just
`price × qty`. Products that can exceed `i64` (a client's quantity before the size check,
fee products, liquidation keys) are computed in `i128` and checked on the way back; the rest
are plain `i64`, kept in range by RISK.md 2.4 and checked for overflow (first update below).
The types were aliases of `i64` until 2026-10-01; since then they are newtypes over `i64`,
and lots × ticks = micros is the one product between two units (the newtypes update below).
The identifiers became types of their own the same day (the last update below).

**Context:** The engine must be exact and deterministic: conservation checks compare sums
to the last micro-dollar, and replay must reproduce every value bit for bit.

**Options considered:**
- *Floating point.* Rounding error makes exact conservation checks impossible, and results
  can depend on evaluation order. Rejected.
- *A decimal library.* Exact, but a dependency, and slower on the hot path. Rejected.
- *Integers with a per-market scale factor.* Needed only if tick × lot isn't a whole number
  of micro-dollars. Not needed (evidence below).
- *Integers with tick × lot = 1 micro (chosen).*

**Choice and evidence:** Polymarket's instrument list (`GET /v1/info/instruments`, fetched
2026-09-29) has 88 instruments, and for every one `price_decimals + quantity_decimals = 6`.
One tick (10^-price_decimals dollars) times one lot (10^-quantity_decimals units) is
therefore always 10^-6 dollars, one micro. Example, SP500-USD: tick 0.1, lot 0.00001;
one unit at 7,502.4 is 75,024 ticks × 100,000 lots = 7,502,400,000 micros = $7,502.40.
That example is the unit test `types::tests::notional_is_price_times_qty_and_reports_overflow`.

Aliases rather than newtypes (the choice until 2026-10-01): price and quantity arithmetic
reads naturally, which matters because the code must be easy to read aloud. The cost is that
the compiler won't catch a price used as a quantity; the equivalence and invariant tests are
the safety net.

Rounding rules for margin and fees are decided in Milestone 2, where they first arise.

**Trade-offs:** While the types were aliases, no compile-time unit safety; since 2026-10-01
the compiler keeps the three units apart, but not two values of one unit (a cost basis and
a collateral amount are both `Micros`: `SlotMoney`'s job, below), and scaling by a rate is
written on the bare numbers. Markets whose tick × lot isn't one micro can't be represented
without adding a scale factor.

**What would change it:** An instrument where tick × lot ≠ 1 micro (add a per-market scale).
(A unit mix-up reaching a test failure would have meant newtypes; they came first, on a
review's advice, below.)

**Update (2026-10-01): overflow checks in every build.** A review pointed out that most of
the engine's products and sums are plain `i64` (`pos × mark`, `locked + pnl`, `locked +=
amount`), safe only because RISK.md 2.4's size limits keep them in range, and that release
builds had overflow checks off: had a limit ever been broken, a benchmark build would have
wrapped silently where the tests (debug builds) panic. The engine crate now has
`overflow-checks = true` in the release and bench profiles, so "never wraps" holds in every
build. Measured with `risk_cost/engine` on the local Ryzen (noise between identical runs
about ±8%): 252, 286, 293 and 294 ns per command without the checks, 292, 278 and 266 ns
with them; no difference that this machine can resolve. The same review reopened the
aliases-or-newtypes choice; see the next updates.

**Update (2026-10-01): slot money and order sequence numbers.** The same review found two
mix-ups that newtypes for `Price`, `Qty` and `Micros` could not catch. The first is in the
money functions, which took a slot's values one by one: `equity` a cost basis and a
collateral amount, `release_amount` three `Micros`, `top_up_needed` two, so swapping two of
them compiled. Cost basis and collateral are both `Micros`, and a type can't tell apart two
values of the same unit, so the money functions now take the slot whole: `money::SlotMoney`
holds a slot's position, cost basis and locked collateral, and unrealized PnL, equity, the
liquidation check, the top-up and the release are its methods (`Slot::money()` builds one).
The top-up and the release now compute the slot's equity themselves, at the mark they are
given, instead of taking it as an argument; every caller had passed exactly that slot's
equity at that same mark, so no value changes. The liquidation keys are `SlotMoney`
methods too (`liquidation_key`, `liquidation_key_by_search`), and `apply_change` takes the
position and the change each whole, as a `money::Holding` (a quantity and its cost), and
reads `position.apply_change(change)`: neither its two `Qty` nor its two `Micros` can be
swapped on their own any more (two whole `Holding`s still could). The second is order ids:
`order_id(account, seq)` took two `u32`s that are different kinds of number, so the
sequence number gets its own type, `types::OrderSeq`, a `u32` newtype whose `Debug` and
`Display` print the bare number: `order_id` takes one, and `sequence_of` returns one, so an
account can't be passed where the sequence number goes. Evidence: the four reference
journals recorded before the change replay with identical events and state, and the unit
tests check the same worked numbers. The liquidation keys and `Holding` came last, after
the newtypes below; the same four journals replay identical after them too.

**Update (2026-10-01): newtypes.** The same review's main point: with `Price`, `Qty` and
`Micros` all aliases of `i64`, a price passed as a quantity, or an amount of money added to
a price, compiled. Giving each unit a type of its own is also the usual practice in Rust, so
they are now `#[repr(transparent)]` newtypes over `i64` with a private field
(`engine/src/types.rs`): `Price::new(ticks)` and `.ticks()`, `Qty::new(lots)` and `.lots()`,
`Micros::new(micros)` and `.micros()`, and no implicit conversion. Values of one unit
compare, add and subtract (a difference of two prices is a number of ticks, so a `Price`),
and `Qty` and `Micros` also negate, take `abs()` and sum. The one operation between two
units is this decision's identity: `qty * price` (or `price * qty`) is `Micros`, a plain
`i64` product that panics on overflow (first update). Everything else goes through the bare
numbers, written out with a comment where it happens: scaling by a rate in ppm (the band's
edges), dividing by a leverage (the margins), micros over ticks for lots (`max_qty`, and a
flow's lots for a notional), and the widenings to `i128` (`i128::from(price)`). A type can't
tell apart two values of the same unit, so swapping a cost basis and a collateral amount is
still `SlotMoney`'s to prevent (previous update), not the types'. Outside the engine:
`Debug` and `Display` print the bare number, as an `i64` does, so every report, log and
snapshot reads byte for byte as before; `repr(transparent)` keeps the records' layout, and
the codecs read and write the same words (`.ticks()` out, `::new` in). The Polymarket
flow's calibrated table is generated by `tools/calibrate/rust.py` as bare integer literals
and checked byte for byte, so its fields stay `i64` ticks and micros; the flow gives each
its unit where it reads it. In that flow a count of real ticks (a spread, a gap) is now a
bare `i64` and a price a `Price`, two things the aliases had let mix. Unit arithmetic in
the other crates now goes through the engine's operators, so it panics on overflow there
too rather than wrapping. Converting every crate found no unit bug: one test added a price
to an amount of money, right only through the identity; it now reads `lots(1) * mark`.
Evidence: the four reference journals replay with identical events and state, a fresh
smoke run replays identically and its audit has no failure, and every test passes. Cost,
`risk_cost/engine` on the local Ryzen: 296, 254 and 279 ns per command with the newtypes,
against 252 to 294 before and 269 and 320 for the aliases measured in between on the same
day; no difference this machine can resolve.

**Update (2026-10-01): identifier types.** The last aliases were the identifiers, and each
had the width of other numbers: `MarketId` was a `u16` like `max_leverage`, `AccountId` a
`u32` like a sequence number or a count, `OrderId` a `u64` like a nonce or a timestamp. They
are now `#[repr(transparent)]` newtypes with a private field, as `OrderSeq` already was
(`engine/src/types.rs`): `::new(n)` names one and `.get()` reads the number back; `Debug` and
`Display` print the bare number, and equality, ordering and `Hash` are the integer's. An id
is a name, not a number, so there is no arithmetic and no conversion. `order_id`,
`account_of` and `sequence_of` build and read the bits through `.get()`. Two helpers beyond
that: `AccountId::MAX`, the id the insurance fund and a rejected operator command carry
(RISK.md 3.4), and `.index()` on a market or account id, its position in a list kept by id
(the engine's markets, a generator's per-account counters), which names the one conversion
to `usize` instead of a cast at every such list. Where an id meets bytes (the CMD40 and
EVT56 codecs, the meta word, the signed message and the EIP-712 encoding, `keys.txt`, the
salt table's hash) the code reads `.get()` and writes `::new` explicitly, so every byte is
as before. `MarketId` and `AccountId` keep `Default`, because the salt table's slots and a
report's per-market counts derive it. The generated Polymarket table writes its market ids
as bare literals, so `profile::Market::id` is now a `u16`, and `Market::market_id` makes it
a `MarketId` where the flow reads it. The stricter types found no id bug, but four places
where an alias had let another kind of number pass: the reference book's `cancelled` took
its market as a bare `u16`; counts were typed as ids (`MarketFlowConfig::markets`, loadgen's
`FlowConfig::accounts`, a pipeline test's market and account counts, and an engine
regression test's `TRADERS_PER_SIDE`), and are now plain `u16` and `u32`; the flows and the
risk benchmark made account ids by adding to a base id (`TAKER_BASE + i`), so the bases are
now account numbers (`u32`) and the ids are made with `AccountId::new`, the same ids as
before; and the generators that derive something from an id's number (a market's class is
`id mod 3`, its start price and random streams come from its id, a gateway is
`account mod N`) now say so with `.get()`. Noted, not changed: a `Reject`'s order id is 0
for a command that names no order (the engine's `NO_ORDER`), which is also the id of
account 0's order 0. Evidence: the four reference journals replay with identical events and
state; a fresh M3 smoke run and a fresh Polymarket-shaped smoke run replay identically, pass
the audit with no failure and write `keys.txt` byte for byte as the runs before the change;
every test passes, among them one that checks that an id prints, hashes and sorts as its
integer. Cost, `risk_cost/engine` on the local Ryzen: 268, 258 and 271 ns per command,
inside the 252 to 320 measured for the updates above.

---

## D-005 Fixed-size command and event records  (M0, 2026-09-29)

**Decision:** Every command and event is a small `#[repr(C)]` struct, and `Command` and
`Event` are `#[repr(C, u8)]` enums over them. A command is at most 40 bytes and an event at
most 64, both checked at compile time. Order ids encode the owning account, and leverage
tiers are one command per tier.

**Context:** These records cross thread rings hundreds of thousands of times a second, so
they must be fixed-size and allocation-free, and small enough that one event fits a cache
line.

**Options considered:**
- *Plain Rust enums (default layout).* Also fixed-size, but the compiler may reorder
  fields, so the size can't be reasoned about or pinned as precisely. Rejected.
- *Serialized messages (e.g. serde).* Allocation and parsing on the hot path. Rejected.
- *One `SetMarketParams` carrying the whole tier table.* Polymarket markets have up to 8
  tiers, which would push every command past 100 bytes. Rejected in favour of one
  `SetRiskTier` per tier.
- *`#[repr(C)]` records (chosen).*

**Choice and evidence:** `engine/src/command.rs` and `engine/src/event.rs`; the compile-time
asserts `size_of::<Command>() <= 40` and `size_of::<Event>() <= 64`. The largest event,
`Fill`, is 56 bytes because it carries order ids only: the owner is `account_of(order_id)`.
Carrying both account ids as well would take a fill to 72 bytes, past the cache line.

Encoding the account in the order id (high 32 bits: account, low 32: per-account sequence)
also makes ids globally unique without coordination, and lets the gateway check that the
signer owns the order by comparing two numbers.

What `repr(C)` does *not* give is a wire or journal format. The records contain padding
bytes (7 after the command tag, for example), which are uninitialized, so copying their
memory to disk would write arbitrary bytes. The workspace also denies `unsafe`, so no code
reinterprets them anyway. The Milestone 3 journal therefore encodes each field
explicitly (little-endian, field by field), and decoding validates every field (`bool`
and enum fields have invalid bit patterns). "At most 64 bytes" means one cache line only
when stored 64-byte aligned. The Milestone 3 ring's slot type will be `#[repr(align(64))]`,
asserted at compile time. (Review finding, 2026-09-29.)

**Trade-offs:** An explicit encoder and decoder to write and test in Milestone 3. Clients
are limited to 2^32 orders per account.

**What would change it:** A field that must grow past these sizes.

---

## D-006 Dependency policy: few, pinned, old enough, reviewed  (M0, 2026-09-29)

**Decision:** Keep the dependency tree small and write the small things ourselves; pin every
direct dependency to an exact version, and every locked version at least two weeks old;
turn off default features; read all code that runs at build time before the first build;
gate on `cargo audit`, `cargo deny` (including an explicit ban list) and `cargo vet`.

One exception to the two-week rule: a release that fixes a published security advisory
may be taken early, once its build-time code has been read. `check-ages.sh` accepts such
versions through `ALLOW=name@version` and reports them as allowed rather than silently
passing them. This came up in M0: `cargo audit` flagged rustls 0.23.44
(RUSTSEC-2026-0285), and the fix, 0.23.45, was at the edge of the window.

**Context:** D-001's concern. The engine is the part people will read, so it should have no
runtime dependencies at all.

**Options considered:**
- *Take crates with default features.* Would pull in, for example, rustls's `aws-lc-rs`
  backend (a large C build), tokio's proc-macros, and criterion's plotting and rayon.
  Rejected.
- *Write everything ourselves.* Impractical for TLS and WebSockets, and a benchmark harness
  with sound statistics is real work. Rejected.
- *Minimise and pin (chosen).*

**Choice and evidence:**
- `engine` has no runtime dependencies; its only external crates are proptest's 15, and
  only for tests. `loadgen` has none: its random-number generator is SplitMix64, 12 lines,
  checked against the reference implementation's published outputs.
- The recorder (52 crates) uses tokio without its macros, tokio-tungstenite, and rustls with
  only the `ring` backend. It stores frames raw, so it needs no JSON library; dates are
  computed with Hinnant's algorithm rather than a date crate.
- The REST history puller is a shell script using curl and jq from Debian.
- criterion (44 crates in the bench crate) is the one heavyweight, kept because the spec
  names it and its statistics are the evidence for performance claims. It is dev-only and
  builds only in the offline container.
- `deny.toml` bans `aws-lc-rs`, `aws-lc-sys`, `openssl-sys`, `native-tls` and the lookalike
  `libsecp256k1`, so they can't come back in through a transitive dependency.
- 118 packages are locked, 96 of which compile on Linux x86_64. Upgrade-delay check,
  build-time code review, and audit/deny/vet results: `docs/SUPPLY-CHAIN.md`.

**Trade-offs:** More code of our own to test (PRNG, date formatting, a minimal channel
scanner in the recorder). Pinned versions need deliberate upgrades.

**What would change it:** criterion's tree growing, or any audit/vet flag on it, would move
microbenchmarks to a small in-house harness built on HdrHistogram (needed in Milestone 3
anyway).

---

## D-007 Polymarket recorder and history puller design  (M0, 2026-09-29)

**Decision:** A *tickers* connection subscribes to `tickers::all` and discovers instruments
from the ticker frames. Each new instrument is handed to one of several *instruments*
connections (45 instruments each), which subscribe to its `book::` (20-level snapshots)
and `trades::` channels. Book and ticker frames are sampled to the **last frame of each
second of server time**; trades and subscription replies are all kept.

Frames are stored byte-for-byte in hourly JSONL files, gzipped when the hour closes, with
`file_opened`, `connected`, `disconnected`, `subscribe` and `subscribe_refused` markers. A separate shell script pulls
Polymarket's published mark history (1 s buckets) and funding history over REST every
hour.

**Context:** The deferred fidelity work (INFO.md 12.1) needs whole hourly funding windows of
live inputs (book, trades, index and mark) plus Polymarket's published outputs to compare
against. Data must accumulate from Milestone 0, and it can't be collected again later.

**Options considered:**
- *Fetch the instrument list over REST and subscribe to a fixed set.* Needs an HTTP client
  in the recorder, and misses new listings. Rejected in favour of discovery from tickers.
- *One connection for everything.* Polymarket accepts at most 100 subscriptions per
  connection. The first version did this, and 37 of 88 instruments silently got no book
  (38 no trades): the replies said "subscription limit reached", and the recorder didn't
  read them. Found by review the same day. Replaced by several connections, and the
  recorder now logs and counts any refused subscription.
- *Parse frames into our own format.* Loses fields we don't know about yet (frames already
  carry an undocumented `ets` field), and needs a JSON library. Rejected: raw frames plus
  receive time are enough.
- *Poll REST tickers.* The docs say REST tickers can be up to 10 s stale. Rejected for live
  data; REST is used only for the published histories.
- *Keep every frame.* Measured with all 88 instruments: about 105,000 frames a minute.
  From the earlier single-connection measurement (~670 KiB/s with 50 books), that is well
  over 55 GiB a day raw, against 309 GB of free disk. Rejected.
- *Keep at most one frame per second of elapsed time.* The second version did this. Kept
  frames drifted to about 1.06 s apart, leaving about 6% of seconds with no sample.
  Rejected (review finding).
- *Keep the last frame of each second of server time (chosen).* This matches how Polymarket
  buckets its published mark history (bucket T holds the last value in [T, T + 1 s)).
  Book frames are full snapshots, not deltas (checked: consecutive frames each carry up
  to 20 levels per side), so dropping the others loses nothing.

**Choice and evidence:** Endpoints and message formats come from
https://docs.polymarket.com/perps/realtime-updates.md and
https://docs.polymarket.com/perps/market-data.md (read 2026-09-29). Public data was
reachable from this machine the same day.

Measured on the recorded data, 2026-09-29 (15:xx UTC):
- Channels: all 88 ticker and all 88 book channels present, plus trades for every
  instrument that traded.
- Sampling: exactly one frame per second per sampled channel (0 duplicate and 0 missing
  seconds over a 130 s window). Every line is valid JSON.
- Volume: about 5.8 MB written per minute, roughly 8 GiB/day raw and about 2 GiB/day
  gzipped (gzip ratio about 4:1 on this data).

Robustness:
- A 30 s connect timeout, and a reconnect if no data frame arrives for 60 s.
- Backoff from 1 s to 60 s.
- One write per line, a repaired newline when reopening a file an earlier run left
  mid-line, and `gzip -f` so an interrupted compression is redone.
- Hours only move forward, so a clock step back (WSL2 after sleep) keeps writing to the
  current file. An hour already compressed is never reopened: a restart that lands in it
  writes `HH.1.jsonl` beside it. Otherwise `gzip -f` would replace the full hour with the
  few new lines (review finding).
- Any panic exits the process, so Docker restarts it. Otherwise tokio would catch a panic
  in one instruments connection's task, and that connection would stay dead unnoticed.
- Each subscribe request is written as a `subscribe` marker (`id=N` plus its channels) and
  each error reply as `subscribe_refused` with the connection's name, since replies carry
  only the request id and ids restart on every connection. A refused channel is not
  retried until the connection reconnects.
- Non-JSON frames are stored escaped rather than breaking the line format.
- Unit tests cover the sampler, frame scanning, date arithmetic, file naming, newline
  repair and link assignment.

The REST puller:
- Stores only mark buckets that closed at least 10 s ago. The API can return the
  still-open bucket, which the first version saved as final.
- Pages funding backwards, because the API returns it newest-first even when given a
  start time. A series' first fetch walks its whole history (bounded at 10,000 pages);
  later ones stop at the cursor, with at most 50 pages per cycle.
- Retries HTTP 429 with backoff.
- Advances each cursor only after the data is written, replacing it atomically. So
  delivery is at least once; de-duplicate by timestamp when loading. Before appending, it
  ends a partial last line left by a failed append.
- The one cycle that ran before the closed-bucket rule (2026-09-29, from 14:37 UTC) could
  have stored an open bucket as final. All mark history was fetched again with the fixed
  puller and compared: of 821,003 points, 15 differed, each the last bucket that cycle
  stored for its instrument (14:37-14:45 UTC), and none were missing. The re-fetched data
  replaced the old.

**Trade-offs:**
- One-second sampling can't reproduce a 200 ms mark calculation exactly (C2's best
  bid/ask between samples is missing); the fidelity report will state this.
- `recv_ms` is this machine's clock, about 0.4 s behind Polymarket's on 2026-09-29, so
  analysis must align on each frame's own `ts`.
- Lines are not in time order across channels (a second's sample is written when the next
  second starts).
- A process restart shows as a `connected` marker with no `disconnected` before it, and
  can lose up to one second of buffered lines.
- The instrument-id scanner assumes the documented `"iid":<number>` field.
- About 2 GiB a day of disk, with nothing deleting old data.

**What would change it:**
- The fidelity work needing 200 ms resolution: subscribe to `bbo::` at full rate, which is
  small.
- Disk use too high: sample the book every 5 s.
- A need for sequence-gap detection: parse `sq` per channel.

---

## D-008 Order-book semantics, defined by the reference book  (M0, 2026-09-29)

**Decision:** The naive `ReferenceBook` is the executable specification of the order book.
The fast book (Milestone 1) must emit identical events and reach identical states on any
command sequence.

**Context:** A fast book is full of indexes and caches, which is exactly where bugs hide. A
slow book that is obviously correct turns "is the fast book right?" into "does it agree with
this one?", which a property test can check millions of times.

**Options considered:**
- *Hand-written unit tests only.* Cover the cases someone thought of, not the combinations.
  Kept as readable examples, but not sufficient alone.
- *Compare against another engine.* Different semantics (self-trade handling, modify
  rules) would make every mismatch ambiguous.
- *Reference model plus property tests (chosen).*

**Choice and evidence:** The rules are listed at the top of `engine/src/reference.rs`:
price-time priority, fills at the maker's price, self-trade prevention by cancelling the
resting order, post-only rejected if it reaches the best opposite price whoever owns it,
IOC remainder cancelled, a fixed order of validation checks, and the modify rules.
`engine/src/reference.rs` has a unit test for each rule, including the validation order
for both place and modify, and `engine/tests/book_equivalence.rs` compares books on random
sequences.

**What "duplicate" means at the book level.** The book rejects an id only while an order
with that id is resting in *this* book. Once the order has filled or been cancelled, the
id is accepted again. INFO.md's stricter rule, that an account's sequence number may never
be reused, spans all markets, so a per-market book can't own it. The engine enforces it
before an order reaches any book (Milestone 2/3), and it will have its own tests there.
(Review finding, 2026-09-29.)

**Modify rules (confirmed with the owner at the start of Milestone 1, 2026-09-29).**
- A modify's quantity is the order's new **total size**, including what has already
  filled (the FIX convention). What is left to fill becomes `new_size - filled`.
- If that is zero or less, the order is removed: `Cancelled { SizeBelowFilled }` with the
  quantity that was still resting.
- Same price and a smaller or equal size shrinks in place and keeps priority.
- A price change or size increase is cancel-and-replace: back of the queue, may trade
  like a new order, and keeps counting what it has filled.
- A post-only modify that would cross is rejected, and the original stays.

Why total size, not remaining: a pre-signed modify can't see fills that happened after it
was signed. Example: 10 lots, 6 fill, 4 remain. A modify to (same price, 8) was meant as a
shrink. Under remaining-quantity semantics it is an increase against the remaining 4: the
order loses priority and exposure grows back to 8, 14 lots in all. Under total size it
leaves 2 to fill, keeps its place, and the client never ends up above the 8 they asked for.
The synthetic market makers (INFO.md section 7) modify without seeing fills, so this
matters in the Milestone 3 load. The cost is one more field per resting order (`filled`),
which the snapshots compare, so the property test checks it too. Considered and rejected:
remaining quantity (the overshoot above), and remaining quantity plus an "expected
remaining" guard that rejects stale modifies (safe, but the extra 8-byte field would push
`Command` past its 40-byte limit, and clients would have to retry).

**Cancelling one account's orders** (`cancel_account`, for liquidation in Milestone 2):
oldest first, by when each order started resting, where a cancel-and-replace counts as
starting again. The fast book keeps a per-account list in exactly that order (INFO.md
section 4), so it needs no sort.

**Trade-offs:** Reference and fast book can share a misunderstanding of a rule. The unit
tests and this entry are the guard, because they state each rule in plain words.

**What would change it:** A rule change (e.g. a different self-trade policy) changes the
reference first, then the fast book follows.

---

## D-009 Property-test configuration  (M0, 2026-09-29)

**Decision:** proptest runs 512 random scenarios of up to 200 commands each, over four
accounts. Prices are mostly in a narrow band so orders interact, but include both ends of
the price range and a few invalid ones. About 1 in 7 commands is malformed (bad quantity
or price) or reuses an earlier id, which the book rejects only while that id is still
resting (D-008); more cancels and modifies hit orders that have already gone. After every command it
compares events, resting orders and the best bid and ask. Failure persistence is off: a
failure's shrunk case becomes a named regression test.

**Context:** The equivalence test only finds bugs if the generated flow is dense enough for
orders to cross, partially fill, self-trade and be modified.

**Options considered:**
- *Wide prices, many accounts.* Orders mostly rest and rarely interact. Rejected.
- *proptest's regression files.* They would be written into the source tree, which is
  read-only in the build container by design (D-001), and they are opaque seeds rather
  than readable cases. Rejected.

**Choice and evidence:** `engine/tests/book_equivalence.rs`. The harness is shown to catch
bugs: with a planted bug (the stub silently ignored cancels of orders larger than 8 lots),
it failed in 0.03 s and shrank the case to a short sequence ("events differ at step 9").
The bug was then removed. Case count and scenario length may rise in Milestone 1, when the
real book is under test.

**Trade-offs:** A failure seen once must be turned into a test by hand.

**What would change it:** If shrunk failures turn out hard to reproduce, persist them to a
file under `/target` instead.

**Milestone 1 update** (2026-09-29), once the real book was under test:
- Two properties, both over four accounts with scenarios of up to 200 commands:
  - 512 scenarios on a 21-tick market (prices mostly in the middle 11, both ends, a few
    invalid);
  - 256 scenarios on a 300,000-tick market, with prices clustered around the level
    index's word and layer boundaries (offsets 63-65, 4,095-4,097 and 262,143-262,145
    from the bottom, both ends) plus a dense band in the middle. In these runs the best
    price moves into another layer-0, layer-1 or layer-2 word in 2.5%, 10.5% and 3.3% of
    steps.
- Steps: place (GTC or IOC, post-only or not), place with a reused id, cancel, modify,
  and `cancel_account` (1 step in 20). Cancels and modifies target a resting order 9 times
  in 10, and 3 places in 4 are moved just short of the opposite best price, so a scenario
  keeps about 7-8 orders resting and some price holds 3 or more orders in 14-21% of steps.
- Modify sizes are worked out from the target's state, so every branch is hit (share of
  modifies, narrow run): removed at or below filled 6.8%, shrink in place 9.9%, same size
  14.4%, increase 11.5%, passive price change 14.2%, crossing requeue 14.3%, post-only
  crossing reject 3.8%, and the reject paths.
- After every step: events, the snapshot (including `filled`), best bid and ask against
  the reference and against the snapshot, not crossed, and `Book::assert_consistent`
  (every link, index, map entry and the free list; at the end of each scenario only in the
  wide run, because it walks all 300,000 levels).
- Evidence it bites: four bugs planted one at a time in the fast book (`cancel_account`
  newest-first, a stale best price after a level empties, a size increase keeping its
  priority, a partial fill not counting `filled`) each failed the property; the file was
  restored by checksum afterwards. A one-off stress run of 20,000 narrow and 5,000 wide
  scenarios (152 s) found no difference between the books.
- Runtime: about 6.5 s for both properties in a debug build.

---

## Notes for later decisions

- **M2 price band.** In Polymarket's instrument list, `price_bounds` equals
  `1 / max_leverage` for all 88 instruments (0.02 at 50x, 0.10 at 10x, and so on; fetched
  2026-09-29). Our derived safe bound (INFO.md section 4, "Price band") is tighter:
  `2 × (band + taker fee) < 1 / max_leverage`. The M2 decision should explain the difference.
  (Addressed in D-015.)
- **M2 leverage tiers.** Polymarket's tiers are keyed by notional lower bound and have up to 8
  rows (SP500-USD: 50x from $0, 25x from $500k, ... 1x from $100M). (Addressed in D-014.)

---

## D-010 Fast order book: tick array, level bitmap, slab, intrusive lists  (M1, 2026-09-29)

**Decision:** The production `Book` (`engine/src/book.rs`) keeps one level per tick of the
market's price range in an array, a hierarchical bitmap per side to find the next
non-empty level (`engine/src/level_index.rs`), cached best prices, orders in a slab of
fixed 56-byte slots with a free list, and two intrusive doubly linked lists per order (its
price level, oldest first, and its account's orders, in resting order). Links are `u32`
slot numbers, not pointers. Within its reserved capacity it never allocates.

**Context:** INFO.md section 4 asks for O(1) cancel, cached best prices, a next-level
search that never scans every level, no heap allocation per order, a deterministic O(k)
cancel of one account's k orders, and millions of operations a second for the book alone.
It also has to be explainable line by line.

**Options considered:**
- *`BTreeMap<Price, VecDeque<Order>>` per side.* Short to write, but O(log n) per level
  lookup, a heap allocation per new level, and cancel needs a search inside the level.
  Rejected.
- *Scan for the next level in the tick array.* Fine near the top of the book, but a sweep
  or cancel that empties the best level on a sparse book scans up to the whole range.
  Rejected: INFO.md rules it out.
- *Pointers (`Rc<RefCell<..>>` or `unsafe`).* Harder to read, `unsafe` is denied
  workspace-wide, and indices into one `Vec` are as fast. Rejected.
- *A per-account `Vec` of order ids for `cancel_account`.* Removing from its middle is
  O(k), and it allocates. Rejected for the intrusive list.
- *Tick array + hierarchical bitmap + slab + intrusive lists (chosen).*

**Choice and evidence:**
- Structure and costs are in the module docs of `book.rs` and `level_index.rs`. The
  bitmap has one bit per level and summary layers above it (bit j of layer k+1 is set when
  word j of layer k is non-zero), so the next level is found in at most 4 steps for 2^24
  levels, using `trailing_zeros`/`leading_zeros`.
- Equivalence with the reference book: D-009 (Milestone 1 update). `level_index` has its
  own tests at every word and layer boundary and a randomized comparison with a
  `BTreeSet`.
- Speed (docs/BENCHMARKS.md, M1): 16.4 M commands/s on a book of about 9,100 resting
  orders, 406 times the reference book; 23.2 M/s on the shallow M0 flow.
- **Allocation** (`engine/tests/no_alloc.rs`, with a counting allocator): zero heap
  allocations over 10,000 mixed commands that hit every path, and under 500,000 steps of
  place/cancel churn at 88-100% of `order_capacity`, and with 10,000 accounts coming and
  going. Two review findings shaped this: std's `HashMap` resized under churn at 950 of
  1,024 reserved orders (it leaves "tombstones" when entries are removed, and clears
  them without allocating only while at most half full), so both maps now reserve twice
  `order_capacity`; and the account map kept every account ever seen, so an account now
  leaves it with its last resting order.

**Trade-offs:**
- Memory grows with the price range, not the order count: 8 bytes per tick (128 MiB at
  the 2^24-tick limit), plus about 2 bits per tick of bitmap. Ranges wider than 2^24 ticks
  are refused; they would need a sorted level structure ("Later").
- Both maps reserve twice the order capacity (about 70-140 bytes per order in all), and
  a map occasionally rehashes in place, which is one O(capacity) call now and then.
- On the deep flow the fast book costs about 40% more per command than on the shallow one
  (61 vs 43 ns), although none of its steps depend on depth. Likely a larger working set
  (about 510 KB of slots plus the maps) against a 512 KB L2 cache; not profiled.
- A cloned `Vec` keeps its length but not its spare capacity, so a cloned book can
  allocate sooner than the original. The benchmark's margin is recorded in BENCHMARKS.md.

**What would change it:** A market whose price range exceeds 2^24 ticks (use a sorted
level structure for it). Profiling showing the per-order hash lookups dominate (give each
account a dense index in M2, so unlinking from its list needs no map lookup).

---

## D-011 Seeded hash for client-chosen order ids  (M1, 2026-09-29)

**Decision:** The book's two maps (order id to slot, account to list) are std `HashMap`s
with our own hasher (`engine/src/id_hash.rs`): the splitmix64 finaliser of
`key ^ seed`. The seed is a per-deployment secret, which the Milestone 3 journal header
will record so that replay rebuilds the same maps. `Book::new` and
`BookOptions::default()` use seed 0 and are documented as tests and benchmarks only.

**Context:** Order ids are chosen by clients. With a public, fixed hash, a client can
pick ids that all land in one bucket, and every lookup then walks that bucket's probe
sequence (hash flooding). The engine must also stay deterministic, so the hash can't
change behaviour.

**Options considered:**
- *std's default SipHash with a random key.* Resistant, but it brings randomness into the
  engine at start-up, and it does more work per 8-byte key (not measured here). Rejected.
- *FxHash or aHash from crates.io.* A new dependency (D-006), and the classic FxHash's single
  multiply leaves the low bits (which pick the bucket) depending only on the key's low bits.
  Rejected.
- *A fixed public hash.* Measured by the reviewer: with seed 0, brute force over about
  33 M sequence numbers (under a second) found 4,000 increasing ids for one account whose
  hashes share their low 13 bits. On those, cancel went from 31 ns to 437 ns (14 times)
  and place from about 100 ns to 920 ns (9 times). Rejected for production.
- *Seeded splitmix64 finaliser (chosen).* Two multiplies, and every output bit depends on
  every input bit.

**Choice and evidence:** `id_hash.rs` tests: the reference splitmix64 value, the seed
changing every hash, structured ids spreading over the buckets, and debug output never
showing the seed. The engine never iterates these maps, so the seed can't change any
event or state; `capacity_and_hash_seed_change_nothing_but_speed` in `book.rs` checks
this over 5,000 mixed calls.

**Trade-offs:** Not a cryptographic MAC. A client able to learn about the seed, by timing
very many requests, could still aim collisions; the seed should be rotated if that is
suspected. The seed must be kept out of logs (the `Debug` implementations leave it out).

**What would change it:** Evidence of seed recovery by timing: switch to a keyed
cryptographic hash (SipHash with a secret key), at some cost per lookup.

---

## D-012 Book-layer synthetic flows (assumed parameters)  (M1, 2026-09-29)

**Decision:** Two seeded flows from `loadgen` drive the book benchmarks:
- **Shallow** (`FlowConfig::default()`, unchanged since M0 and pinned by a test to the M0
  command stream): 100 accounts, prices uniform within 20 ticks of a fixed mid, sizes 1-10,
  40% cancels. It keeps a few hundred orders resting.
- **Deep** (`FlowConfig::deep()`): 1,000 accounts, at most 10,000 live orders (then each
  place becomes a cancel of a random live order, as a market maker keeps a fixed number of
  quotes out), distance from mid drawn geometrically with a 95% chance of each further tick
  (mean 19 ticks; integers only, so every machine draws the same prices), 10% modifies
  (half move the price, half set a new total size), 5% of places are IOC takers that cross
  the spread, 20% of GTC places are post-only. In steady state it is about 46% places, 44%
  cancels and 10% modifies, with about 9,100 orders resting on about 120 levels per side.

**Context:** The M0 flow kept the book shallow, which flatters a linear scan (M0
benchmark). INFO.md section 7 says the flow's parameters are assumptions in v1, recorded
here; calibrating them from recorded Polymarket data is "Later".

**Options considered:**
- *Fixed probabilities with uniform random cancels.* The live count drifts up without
  limit or collapses. Rejected for the live-order cap.
- *10% IOC takers.* 7.2% rejects: each fill leaves the open-loop generator an order it
  later cancels or modifies in vain. Lowered to 5%, which gives 5.3% rejects.
- *Uniform prices over a wide band.* Most orders never interact, and real books are dense
  near the top. Rejected for the geometric distance.

**Choice and evidence:** loadgen tests pin the M0 stream and check the deep flow's
determinism, mix proportions (within 1.5 points), sides and total-size modifies. Depth
is printed by the benchmark before and after each timed batch (docs/BENCHMARKS.md, M1).

**Trade-offs:** The mid never moves (a moving fair value comes with the Milestone 3 flow),
so there are no sweeps through many levels. The generator is open-loop and doesn't see
fills, so 5.3% of its commands are rejected (505 cancels or modifies of orders that had
filled, 24 post-only crossings), just above INFO.md section 7's 5% flag; this is stated
rather than tuned away.

**What would change it:** Recorded Polymarket data showing different cancel-to-trade
ratios, order lifetimes or distances from mid ("Later" calibration).

---

## D-013 Isolated collateral model, release rule and rounding  (M2, 2026-09-29, model decided by me; revised after review)

**Decision:** Each account has a free balance. Each (account, market) pair has a slot
holding the position, its signed cost basis, signed locked collateral, `open_buys`,
`open_sells` and the chosen leverage. The pre-trade check tops the slot up from the free
balance by exactly `IM(W') − equity` when that is positive. Realized PnL and fees go to
the slot's locked collateral. At the end of each command, every slot the command touched
returns `max(0, min(locked, equity) − IM(W))` to the free balance. All rounding favours
the exchange, and money that could become withdrawable is rounded down. Full rules:
`docs/RISK.md` sections 2.3, 5.2, 7.3 and 8.2. The release is safe with resting orders
because every resting order stays inside the band of the current mark (D-015).

**Context:** INFO.md section 4 fixes the model (confirmed by me on 2026-09-29) but leaves
the release rule and rounding to M2. The rules must never release unrealized profit, never
leave equity below IM, let a trader with no free balance still close, keep the
conservation identity exact to the micro, and cost O(1) per command.

**Options considered:**
- *Release only when the position is flat.* Collateral stays stuck behind cancelled
  orders and reduced positions. Rejected.
- *Release on every `SetMark`.* Visits every slot on every mark, which INFO.md forbids.
  Rejected.
- *Release `locked − IM(W)`.* When the position shows a loss, this takes equity below IM.
  Rejected.
- *Release `min(locked, equity) − IM(W)` at the end of each command that touches the slot
  (chosen).* Keeps equity at or above IM and keeps unrealized profit in the slot.
- *A stale-price add-on in the release* (review H1): hold back extra collateral for orders
  that a mark move has left far through the mark. It doesn't help a flat slot that no
  command touches between the move and the fill (review F1), and it needs per-slot running
  price bounds. Rejected for cancelling such orders at `SetMark` (D-015).
- *Lock margin per order* (a reservation per resting order). Needs per-order collateral
  state and over-reserves orders that hedge each other; Polymarket's rule is the
  worst-case size, not a sum. Rejected.
- *Round to nearest.* Symmetric, but can understate a requirement by half a micro and round
  a rebate up. Rejected for directional rounding: requirements up, fees toward plus
  infinity (rebates down in size), band edges toward the mark, the removed cost basis up
  (so realized PnL rounds down).

**Choice and evidence:** `docs/RISK.md` 7.3 shows that a fill changes a slot's
`locked − cost` by exactly `−qty × price − fee` whatever the rounding, so conservation is
exact; 8.2 shows the release never goes below IM and never releases unrealized profit;
each has worked examples. The spec's arithmetic was checked on two throwaway integer
models (486,825 random commands on the first draft, 360,000 on the revised spec). In the
repo, the evidence will be invariants I3, I5 and I7 after every command of the engine
property test; the command properties I17 (top-up exact), I18 (release never below IM)
and I19 (release complete), checked with the test's own formulas; and unit tests T2, T3
and T6.

**Trade-offs:** Because top-ups are exact, a new position sits below IM by its own fee
right after it fills (RISK.md Q1). An IOC that does not fill tops up and releases in the
same command. Excess collateral from a favourable mark move comes back only at the slot's
next touch; a `SetMark` releases only for the owners of orders it cancels. `locked` is
signed, which is one more thing to explain.

**What would change it:** My answer to RISK.md Q1 (curing a margin call by a top-up).
Cross margin (INFO.md 12.2). A client command to add or remove margin.

---

## D-014 Leverage default and leverage tiers  (M2, 2026-09-29, 1x default and capping decided by me; revised after review)

**Decision:** A slot's chosen leverage is 1 until `SetLeverage`, which accepts
1 to `max_leverage` and rejects anything else (`InvalidLeverage`). Effective leverage is
`min(chosen, tier maximum)`, where the tier is picked by the notional at the mark of the
size being margined (`W' × mark` in the pre-trade check) and its rate applies to the whole
notional. MM is `ceil(|pos| × mark / (2 × max_leverage))`, flat per market. A tier table
is sent as `count` rows, one `SetRiskTier` each, staged, and committed in one step when its
last row is accepted; a commit is allowed on a live market, because tiers change only IM,
not MM or the liquidation keys. A new or reconfigured market needs a committed table
before its first `SetMark` (`NoRiskTiers`). `SetLeverage` on a live slot is checked like
an order: it tops up, or is rejected if the free balance can't cover the new requirement,
and releases collateral if the requirement falls.

**Context:** Polymarket's instruments have up to 8 tiers keyed by notional lower bound
(SP500-USD: 50x from $0, 25x from $500,000, ... 1x from $100M). Its FAQ says tiers raise
"the initial margin rate on your entire position — the cap is not applied bracket by
bracket", and "MMR = 0.5 / max_leverage, independent of position size and of your
leverage setting". A command holds one row (D-005). The first draft wrote rows one at a
time on an empty market; review F2 showed that a trader can place one order between two
rows, which makes the market non-empty and freezes a cut-down table (for example a single
50x tier for every size).

**Options considered:**
- *Default to the market's maximum leverage.* Most capital-efficient, but a new account's
  first order would carry the most risk without anyone choosing it. Rejected (my call).
- *Reject an order whose notional falls in a tier below the chosen leverage*, making the
  trader lower the setting first. Pre-signed flow can't react to that. Rejected (my call)
  for automatic capping.
- *Bracket-by-bracket tiers.* Not Polymarket's rule. Rejected.
- *Tiered MM.* The FAQ says MMR is flat. Rejected; it would also break the closed-form
  liquidation key (D-018).
- *The whole tier table in `SetMarketParams`.* Too big for a command (D-005). Rejected.
- *Rows written one by one on an empty market* (first draft). The race above. Rejected.
- *Tiers only on an empty market* (INFO.md's wording for market parameters). Any client
  can keep a market non-empty with a 1-lot order, so the operator could never lower
  leverage on a live market. Rejected: tiers don't move keys, so a live commit is safe.
- *Staged rows, committed atomically, live commits allowed (chosen).*

**Choice and evidence:** `docs/RISK.md` 4.3, 4.4, 6.6 and 6.9. Worked example at SP500's
$500,000 boundary: 6,000,000 lots need IM 9,002,880,000 (2%), 8,000,000 lots need
24,007,680,000 (4% of the whole notional). At mark 100,000, 5,000,000 lots is exactly
$500,000 and uses 25x (IM 20,000,000,000), 4,999,999 lots uses 50x (IM 9,999,998,000),
and a chosen leverage of 20 binds below the 25x tier (IM 25,000,000,000). Unit tests for
the boundary and for staging are listed in RISK.md section 13.

**Trade-offs:** IM jumps at a tier boundary, by design. A stored leverage above a lowered
maximum is capped silently. A live tier commit can put slots into margin call with no
event (the proactive `MarginCall` event is "Later"). Every market needs one more command
at setup (a one-row table).

**What would change it:** Signed, user-initiated `SetLeverage` (INFO.md 12.6). A tiered
MM, which would make tier changes move keys and require re-keying every slot.

---

## D-015 Price band: derived bound, tighter than Polymarket; stale orders swept  (M2, 2026-09-29, band rule and sweep decided by me)

**Decision:**
- A buy priced above `floor(mark × (1 + band))` or a sell priced below
  `ceil(mark × (1 − band))` is rejected (`PriceBand`), for every place and every modify
  that replaces an order.
- `SetMarketParams` checks two rules. Rule 1 (decided by me):
  `20 × max_leverage × (band_ppm + max(taker_fee_ppm, maker_fee_ppm)) ≤ 9,000,000`, that
  is `2 × (band + fee) ≤ 0.9 / max_leverage`, INFO.md's bound with a 10% safety margin.
  Rule 2: the exact condition, including the fee on the band and fee rounding,
  `min_price × (10^12 − 2L × (10^6 × (b + f) + f × b)) ≥ 2L × 10^12` (in `i128`), which in
  practice puts a floor of about `20 × max_leverage` ticks on `min_price`.
- After each `SetMark`'s liquidation walk, resting bids above the new upper edge and asks
  below the new lower edge are cancelled (`Cancelled { PriceBand }`) and the collateral
  behind them is released. So every resting order is always inside the band of the
  current mark.

**Context:** After a margin check, equity ≥ `W × mark / max_leverage`. A slot's open
orders can fill at most `2W` in total, each at most `band + fee` worse than the mark, plus
less than 1 micro of fee rounding per fill. So, under rule 2, fills can't take equity
below zero at the mark of the check, and fills that only reduce a position can't at any
command boundary (RISK.md 5.3, claims 1 and 2). Without a band, an order far through the
mark is bankrupt on its first fill, and two colluding accounts can drain the insurance
fund. The first draft checked the band only when an order was placed. Review found that
stale resting orders reopen the hole with client commands only: a bid left at 100,000
while the mark drifts to 80,000 in 1% steps cost the fund 15,000,000 (F1), and a stale
closing order plus a release cost it 7,000,002 at an unchanged mark (H1). Review also
found the 10% margin too thin for fee rounding at marks below about `20 × max_leverage`
ticks (M1: −1,000 micros at mark 100 on a 20x market) and for the `fee × band` term at
`max_leverage` 1 (L1).

**Options considered:**
- *Polymarket's `price_bounds`,* which equals `1 / max_leverage` for all 88 instruments. A
  flip at the band edge can then lose twice the initial margin, so fills alone create bad
  debt. Rejected for v1; matching Polymarket exactly is "Later".
- *An adverse-price add-on to IM,* charged on top of the requirement by distance from the
  mark. Keeps a wide band, but every order's margin then depends on its price. Rejected.
- *Rule 1 alone* (first draft). Fails at small marks (M1) and at `max_leverage` 1 (L1).
  Kept, with rule 2 added.
- *A stale-price add-on in the margin and release rules* (H1's proposal). Doesn't stop F1,
  because a flat slot with only resting orders is never touched between the drift and the
  fill. Rejected.
- *Cancel stale orders lazily,* just before an incoming order would match one. Same
  protection, but the cancels appear inside another account's command, and "every resting
  order is inside the band" no longer holds. Rejected for the eager sweep.
- *Eager sweep at `SetMark` (chosen).* O(1 + orders cancelled) per mark, through one new
  book method (`cancel_beyond`) in both books.

**Choice and evidence:** `docs/RISK.md` 5.3 (the claims, and a table of the largest bands
and smallest `min_price`), 5.4 (the sweep) and 6.8. At 50x with a 400 ppm taker fee the
largest band is 8,600 ppm against Polymarket's 20,000, with `min_price` at least 1,004; at
20x, 22,100 against 50,000, with at least 402. The lowest mark recorded on 2026-09-29
among Polymarket's 88 markets (4,168 ticks, KPEPE-USD, 10x) is far above its floor of
201. Tests: T4 (a taker's own band-edge flip leaves it between zero and MM; it is
liquidated and the fund absorbs non-negative equity), T5 (collusion rejected), T6 (a stale
bid swept at the exact edge, fund unchanged), H1's sequence (fund unchanged), and
invariant I16 (every resting order inside the current band).

**Trade-offs:**
- About 2.3 times tighter than Polymarket at a 400 ppm fee, so aggressive limit orders far
  through the mark are refused.
- A client's order can be cancelled by a mark move (RISK.md Q3).
- **Residual risk, accepted in v1.** Orders that open or grow a position are covered only
  while equity ≥ `W × mark / max_leverage`. After a mark move against the position (the
  slot still above MM, so nothing happens), such orders can fill inside the band and leave
  the slot below zero. The loss is at most about `band + fee` of the filled notional.
  Example (review F3, SP500's live parameters): after an 11.1% fall, a 500,000-lot bid
  fills and the fund loses 224,015,813, 0.67% of the order's notional. Closing this needs
  INFO.md's "Later" margin-call index. A unit test pins the number.
- Apart from that, bad debt needs a gap past a position's bankruptcy price (D-002),
  reported as shortfall.

**What would change it:** The fidelity work (INFO.md 12.1) needing Polymarket's band; the
adverse-price add-on would then make the wider band safe. A "no" to Q3 (then the lazy
check, or accepting the stale-order hole). Measured bad debt from the residual risk in the
M3 load (then build the margin-call index).

---

## D-016 Engine command semantics: validation order, event order  (M2, 2026-09-29, revised after review)

**Decision:**
- For place, modify and cancel, the engine first makes every check the book would make
  (the order's existence through `book.order(id)`), in the book's order, then `NoMark`,
  `PriceBand`, `SizeLimit` and the margin rule. So collateral never moves for an order the
  book then refuses, and the book never emits a `Reject` on the engine's path. The engine
  asserts, in release builds too, that the book's first event is the one it expects. A
  rejected command changes no state, counters included.
- The engine hands the book a scratch event buffer, then walks it: fills get their fees
  and update both slots, cancels adjust open totals, and each book event is followed by
  the events it causes.
- After matching, and after any GTC remainder rests, one pass over the touched slots (the
  command's own slot first, then makers in the order of their first fill, each once)
  liquidates any slot below MM, releases collateral from the others, and re-keys them.
  `SetMark` runs the same pass over the owners of the orders its band sweep cancelled.
- The event order per command is fixed (`docs/RISK.md` section 11). Money moves are
  reported source first. `InsuranceShortfall` appears at most once per command, at its
  end.
- Record changes: `PositionChanged` gains `locked`; `Fill` gains `taker_side`; `Cancelled`
  gains `side` (new fields last, with a size assert per struct); new echo events
  `LeverageSet`, `MarketParamsSet`, `RiskTierSet`; `SetRiskTier` gains `count`; new reject
  reasons `ReservedAccount`, `SizeLimit`, `InvalidAmount`, `InvalidParams`,
  `WithdrawalReserve`, `NoRiskTiers`; new cancel reason `PriceBand`. `Reject.account` is
  `AccountId::MAX` for market-level commands. `Event` stays at 64 bytes and `Command` at 40.

**Context:** The event stream is the replay contract (D-005, M3), and the fast and naive
engines must emit identical streams. INFO.md: liquidation checks run after matching, never
mid-sweep.

**Options considered:**
- *A sink that updates the ledger while the book matches.* No buffer copy and the same
  event order, but ledger code would run inside the book's call. Rejected for the scratch
  buffer: two plain loops.
- *`InsuranceShortfall` after every change inside a command.* More events, and a command
  is atomic anyway. Rejected.
- *Re-key after every fill.* O(fills × log n) instead of O(touched slots × log n).
  Rejected.
- *Keep order sides in engine-side records* instead of adding side fields to events.
  Duplicates the book's state; the fields fit in existing padding. Rejected.
- *Rely on the engine's copy of the book's checks with only a debug assertion* (first
  draft; review R6). In a release build a disagreement would leave a lone `Reject` after
  collateral had moved. Rejected for pre-checking everything, cancel included, and
  `assert!`.
- *Account 0 in rejects of market-level commands* (first draft; review R10). It is also a
  valid client id. Rejected for `AccountId::MAX`, which no client can hold.

**Choice and evidence:** `docs/RISK.md` sections 6, 7.1, 8, 11 and 15; record sizes
rechecked with the `repr(C)` rules (`Fill` 56 bytes with `taker_side` last, 64 if first).
The engine property test compares complete event streams, and a shadow ledger built only
from events must match the engine's state after every command (RISK.md 14.3).

**Trade-offs:** The book's events change shape, so the M1 tests change with them. A sweep
larger than the scratch reservation allocates once.

**What would change it:** Profiling showing the scratch copy matters: switch to the inline
sink, which emits the same events.

---

## D-017 O(1) worst-case check and the naive reference modes  (M2, 2026-09-29, revised after review)

**Decision:** Each slot keeps `open_buys` and `open_sells` as running totals, updated on
place, modify, fill and cancel, so the worst-case size `W` and the margin check are O(1).
The engine is generic over a `Mode` with two compile-time switches, running totals and
the liquidation index. `Engine<ReferenceBook, Naive>`, with both switched off, is the
executable reference, and a property test requires `Engine<Book, Fast>`, and every other
mode, to emit identical events and reach identical snapshots (defined field by field in
RISK.md 15.5). Because all modes share the money formulas, the test adds an independent
checker with its own formulas and a shadow ledger built only from events.

**Context:** INFO.md: "the check is O(1) and never loops over orders or positions". The M2
ablation compares it with a naive loop over the account's resting orders. M1's
`ReferenceBook` is the precedent for a slow, obviously correct reference.

**Options considered:**
- *Only the naive loop.* Simple, but O(k) per order, and market makers with many quotes
  pay it on every requote. Rejected; kept as the reference.
- *A runtime flag.* A branch on every call, and both paths live in production. Rejected.
- *Separate fast and naive engines.* Two copies of the ledger code would drift apart.
  Rejected.
- *Associated constants on a `Mode` type (chosen).* One engine; four functions branch on
  constants that compile away.

**Choice and evidence:** `docs/RISK.md` section 14. 14.3 states what equality between
modes proves (the running totals equal the book's; the index picks the same slots in the
same order as a scan) and what it doesn't (the money formulas, which the independent
checker, the shadow ledger and the unit tests cover).

Evidence it bites (M2, on the final code; each bug planted alone, the file restored and
checked with sha256; time to the first failure of the debug property test, shrinking off):

| Planted bug | Caught first by | Time |
|---|---|---|
| Long key one tick high | the re-key's debug boundary check (RISK.md 9.1) | 0.13 s |
| Ties go to the higher account id | `Fast`'s events differ from the reference's on a `SetMark` | 0.73 s |
| No open-total subtraction on a self-trade cancel | I4 (open totals vs the book) | 0.25 s |
| Release computed as `locked − IM(W)` | the independent checker, I18 | 0.07 s |
| Top-up one micro too large | the independent checker, I17 | 0.06 s |
| `SetMark` sweep skipped | I16 (an order outside the band) | 0.09 s |
| MM rounded down (shared by every mode) | 4 unit tests in `money.rs`; in the property test the re-key check (I10 in release) | 0.10 s |

The last row is the one mode equivalence can't catch, since every mode shares the
formula; the unit tests and invariants did. A release-build run of 10,000 scenarios
(1,982,828 commands) passed. Ablations A and B are in `docs/BENCHMARKS.md`, M2.

**Trade-offs:** Running totals are extra state that must stay exact (invariant I4, checked
against the book after every command). The book gains `open_quantities`, used only by the
naive modes. The snapshot must read open totals through `open_totals()` and leave out
bookkeeping fields (index entries, counters), or the modes would never compare equal.

**What would change it:** Cross margin (INFO.md 12.2), which moves the loop to positions
and gets its own ablation.

---

## D-018 Liquidation keys: exact closed form; an indexed heap per side  (M2, 2026-09-29, extends D-003)

**Decision:** A slot's key is computed with one `i128` division. Long:
`ceil(2L(cost − locked) / (pos(2L − 1))) − 1`, and no key if that is below 1. Short:
`floor(2L(locked − cost) / (s(2L + 1))) + 1`. (`L` = the market's maximum leverage,
`s = −pos`.) Each market keeps two indexed 4-ary heaps behind a small `LiquidationIndex`
type (`engine/src/liquidation_index.rs`): longs ordered by `(Reverse(key), AccountId)` and
shorts by `(key, AccountId)`, so the first entry is the next to liquidate and ties go to
the lower account id. A position map from account to heap slot lets any entry be moved or
removed, not only the first. (First built as two `BTreeSet`s; see "Choice and evidence".) A touched slot is
re-keyed once, at the end of its command, and only if its position, cost or collateral
changed. In debug builds each re-key asserts the boundary and that the key is on the far
side of the mark (`key_long < mark < key_short`).

**Context:** D-003 requires the exact boundary tick, computed with the engine's own
integer equity and MM, so that "key at or through the mark" is exactly "equity < MM".

**Options considered:**
- *A rational estimate corrected by a tick* (INFO.md's wording). Correct, but the
  correction never moves: MM's rounding drops out of the comparison, because an integer is
  below `ceil(y)` exactly when it is below `y`. Kept as a debug assertion.
- *Binary search over ticks with the integer check.* Exact but O(log range) per re-key.
  Used by the naive mode, on the interval between the old and the new mark (every slot was
  safe at the old mark), and by the tests as an independent check of the formula.
- *Floating point.* Rounding mismatches at the boundary. Rejected.
- *Structure:* `BTreeSet` (std, ordered, readable: the first choice); an indexed heap per
  side (never allocates, same walk, our own code: the named fallback, now chosen); an
  array bucketed by tick (keys can fall outside the price range, and memory grows with
  it; rejected).

**Choice and evidence:** `docs/RISK.md` section 9. INFO.md's SP500 example: key 73,100
(7,310.0); a mark of 73,101 does not liquidate (equity 182,820,000, MM 182,752,500) and
73,100 does (182,720,000 against 182,750,000). Invariant I10 in the property test. Review
recomputed the closed forms against a binary search on 300,000 random slots and an
exhaustive small-state scan; `money.rs` tests do the same on 100,000 full-scale slots.

**The switch to the heap** (review P1, RISK.md 18). The rule set in advance (below) fired:
in `no_alloc.rs`'s flow with 256 accounts the `BTreeSet` made 468 allocations and 468 frees
per 10,000 commands, 47 times the threshold, because nearly every order re-keys its slot
and B-tree nodes split and merge. The heap reserves `slot_capacity` entries and makes
none; `no_alloc.rs` now asserts zero for `Fast`. Four children per entry instead of two
halve the depth (10 levels for a million entries). Measured back to back on a loaded
host, so only the ratios mean much (medians of three, `BTreeSet` against heap): a place
480 against 456 ns at 1,000 positions and 548 against 353 ns at 1,000,000; a cancel 450
against 268 ns at 1,000,000; a `SetMark` liquidating 100 of 1,000,000 positions 24 against
56 µs. The heap makes each order cheaper and each liquidation about 2.4 times dearer (the
walk takes the first entry out and moves another down every level); orders are far more
frequent than liquidations.

**Trade-offs:** About 90 lines of our own heap code instead of std's ordered set, with its
own consistency check (`assert_consistent`, part of I10). Liquidations cost more per slot
than with a B-tree (above). The index reserves room in every mode, including those that
don't use it: roughly 50 MB per market at a `slot_capacity` of a million. Keys depend on
the maximum leverage, which is why it can only change on an empty market; tier changes
don't move keys.

**What would change it:** Liquidation bursts (a big mark gap across many positions)
showing in the M3 latency percentiles: then write each moved entry once per level instead
of swapping, or go back to a B-tree for the walk. A tiered MM, which would break the
closed form and bring back the estimate and correction.

---

## D-019 Insurance-fund account: identity, netting, shortfall reporting  (M2, 2026-09-29, extends D-002)

**Decision:** The fund is account `AccountId::MAX`, capitalised by an ordinary `Deposit`.
It has no `Account` entry: its balance is `fund_balance`. It can't place orders, withdraw
or set leverage (`ReservedAccount`). In each market it holds one netted position with a
cost basis, merged with the same position-change function as fills; the realized part
goes to its balance. Its unrealized PnL is a running `i128` total, updated in O(1) per
`SetMark`. Uncovered bad debt, `max(0, −fund equity)`, is reported with
`InsuranceShortfall` at the end of any command that changed it (converted to `i64` with a
checked conversion that panics rather than wraps).

**Context:** INFO.md section 4, "The insurance fund"; D-002.

**Options considered:**
- *An ordinary account exempt from checks that can also trade.* It could unwind its
  inventory, but a trader exempt from margin is a hole. Rejected for v1 (RISK.md Q2).
- *Keep each absorbed position separately.* Exact attribution, but state grows with every
  absorb, and there is no netting. Rejected.
- *Recompute fund equity over every market on each `SetMark`.* O(markets) per mark.
  Rejected for the running total.
- *A separate id space for the fund.* A reserved `AccountId` reuses `BalanceChanged` and
  `PositionChanged` for the fund. Chosen.

**Choice and evidence:** `docs/RISK.md` 3.4 and section 10; tests T1 (the fund gains the
absorbed equity, 182,720,000), T4, and the netting examples of 10.2.

**Trade-offs:** Absorbed positions stay until an opposite absorb nets them, and a market
where the fund holds a position can't be reconfigured with `SetMarketParams` (its tier
table still can, D-014). `AccountId::MAX` is unavailable to clients; rejects of the
operator's market-level commands carry it too.

**What would change it:** A "yes" to RISK.md Q2, or the INFO.md 12.2 waterfall.

---

## D-020 Size and price limits against overflow  (M2, 2026-09-29, extends D-004; bounds tightened after review)

**Decision:** A market's `max_price` must be below 2^32. Each market gets
`max_qty = floor(2^53 / max_price)`, and a place or modify whose worst-case size would
exceed it is rejected (`SizeLimit`). Each computation's integer width (`i64` or `i128`)
is listed in `docs/RISK.md` 2.4; every `SetMarketParams` check is computed in `i128`.
Aggregates (balances, the fund, fees) use checked arithmetic: a `Deposit` that would
overflow is rejected (`InvalidAmount`), and any other overflow panics rather than
wrapping.

**Context:** D-004 computes products in `i128` and checks them on the way back. M2 adds
sums (cost bases, equity, liquidation keys) whose bounds must be argued, and a client can
send any `i64` quantity.

**Options considered:**
- *`i128` for all stored money.* No bounds needed, but twice the state, and events would
  exceed 64 bytes (D-005). Rejected.
- *Saturating arithmetic.* Never panics, but silently wrong numbers break conservation.
  Rejected.
- *Bounds at input plus checked aggregates (chosen).*

**Choice and evidence:** `docs/RISK.md` 2.4 bounds each per-slot value:
`abs(cost) <= abs(pos) × max_price <= 2^53` for longs and shorts alike (a reduce keeps
`floor(cost × s' / s)`, a one-line proof), locked collateral below 2^55 at command
boundaries, equity below 2^56. The first draft's bounds were looser (review L2). Unit
test: a quantity near `i64::MAX` is rejected without overflowing.

**Trade-offs:** One slot's worst-case notional is capped at about $9.0 billion. An
aggregate overflow panic is possible in theory, at balances of trillions of dollars.

**What would change it:** A market that needs a larger notional per slot: raise the limit
(the `i64` headroom allows a few more doublings, each of which needs the 2.4 bounds
rechecked).

---

## D-021 Signed messages: secp256k1 ECDSA via k256, SHA-256, fixed 136-byte format with an expiry  (M3, 2026-09-29, k256 decided by me; revised after review)

**Decision:** A client message is 136 bytes: a 72-byte signed part (magic `PERP`,
version, deployment id, account, nonce, an expiry `expires_at`, then the command in its
40-byte encoding, D-023) and a 64-byte `r || s` signature: ECDSA over secp256k1 through
RustCrypto's `k256` (locked at 0.14.0), over the SHA-256 of the signed part. Only low-S
signatures are accepted, checked by our own byte comparison. Public keys (33-byte
compressed) are gateway state, loaded at start from `keys.txt`; the engine never sees
keys. The gateway checks that the signer's account is the one inside the order id, and
that the message hasn't expired. Keys are made for this protocol only. Spec:
`docs/PIPELINE.md` section 5.

**Context:** INFO.md 3 and 4: gateways verify signatures off the core thread; only an
order's owner may cancel or modify it. D-005: the order id names its account. A signature
must not be reusable across deployments or message types. INFO.md 5a: crypto crates are a
supply-chain target, and `libsecp256k1` is a lookalike of `secp256k1`. The security review
(PIPELINE.md 22, S2) showed that a message that was never forwarded stays usable until its
account's nonce moves on, so whoever kept its bytes could get it executed later.

**Options considered:**
- Library: *`k256`* (pure Rust, RustCrypto; my choice); *Bitcoin Core's C libsecp256k1
  through the rust-bitcoin `secp256k1` crate* (C compiled by a build script, more to
  review; kept as the fallback if M3 shows 100k signed orders/s can't fit, after its own
  review); *the `libsecp256k1` crate* (abandoned lookalike, banned in `deny.toml`).
- Hash: *SHA-256* (`k256`'s default, no extra crate; chosen); *keccak-256 over EIP-712
  typed data*, which Ethereum wallets and Polymarket's CLOB sign (needs `sha3` and a
  typed-data encoder, and v1 has no wallets; rejected for now).
- Format: *fixed binary, explicit fields* (chosen); *JSON or a variable-length encoding*
  (parsing cost, and one order could be signed in several byte forms; rejected); *a domain
  separator that is hashed but not sent*, as EIP-712 does (saves nothing here and gives
  worse reject reasons; rejected for sending and signing it).
- Malleability: *accept both forms of `s`* (anyone could make a second valid encoding of
  any message; rejected); *low-S only* (chosen, as Bitcoin does).
- Lifetime: *no expiry* (a kept copy can be executed at a moment someone else picks;
  rejected, PIPELINE.md Q4); *nonce = the client's clock, accepted within a window*
  (Hyperliquid's approach; no new field, but the benchmark's pre-signed nonces would become
  timestamps tied to the run's clock, and the `NonceJump` rule of D-022 would have to go;
  rejected); *an explicit `expires_at`* (chosen: 8 bytes, still two SHA-256 blocks, one
  compare at the gateway).

**Choice and evidence:** PIPELINE.md 5.1 to 5.6. The worked example of 5.6 (bytes,
SHA-256, key, RFC 6979 signature and its high-S twin) was computed with two independently
written Python models that both reproduce the published RFC 6979 secp256k1 test vector; it
becomes a known-answer test. The `k256` API the spec names was checked against the locked
0.14.0 sources (5.2). Unit tests for every reject reason (PIPELINE.md 18.1). Verify and
sign costs, and verify scaling over cores, are measured by the M3 probes (15.10) and
recorded in BENCHMARKS.md.

**Trade-offs:** 72 bytes of signature and expiry, and one verification (40 to 80 µs,
estimated for `k256` on PERPSBOX) per message, which is why the gateways need several cores
(PIPELINE.md 17). Not wallet-compatible. Keys change only at a restart: replacing a leaked
key stops every account while `keys.txt` is replaced (the journal continues, with the new
registry's digest in the next segment). The signature bytes are not a message id, since
the key holder can always re-sign; the id is `(deployment, account, nonce)`. v1 does not
defend against floods of forged messages (D-031).

**What would change it:** M3 measurements showing that 100k signed orders/s can't fit
with `k256` on the rented box (then the C library after review, or one signature per
batch). Wallet signing (then EIP-712, which wallets display; a wallet key must never sign
our raw SHA-256 digest). Untrusted clients over a network (then D-031's preconditions
first).

**Status (2026-09-30):** The owner's answers of 2026-09-30: every signed message carries
an expiry (Q4), and each signed command's signature and expiry are journaled (Q3). Built
as specified; the registry loader now also refuses any other spelling of the same keys, so
one set of keys has one digest (PIPELINE.md 5.4, 22). Evidence: local validation
2026-09-30 (WSL2, `docs/PIPELINE.md` 22); headline pending PERPSBOX
(`docs/RUNBOOK-PERPSBOX.md`). Also on 2026-09-30, the owner had the C library reviewed and
built as an opt-in second verifier, without waiting for the headline to fail (D-032);
`k256` stays the default.

---

## D-022 Nonces: per account, strictly increasing, bounded jumps, used up at the account's gateway  (M3, 2026-09-29, revised after review)

**Decision:** Every signed message carries a `u64` nonce per account. The account's one
gateway (`account mod N`) accepts it only if it is above the account's last used nonce,
and at most 2^32 above it (gaps allowed), and uses it up when it forwards the message to
the sequencer, after the signature verifies. Once the journal batch holding the message is
durable, the nonce stays used up: gateway rejects don't use one, engine rejects do. After
a restart the state is rebuilt from the durable journal. `StaleNonce` says only that the
nonce isn't above the last one forwarded; it never tells a client that its message
executed. Spec: `docs/PIPELINE.md` section 6.

**Context:** INFO.md 4 leaves gateway or sequencer open. RISK.md 3.1: the engine's order
sequence advances only on accepted places, so a rejected signed place could be replayed
later, when it would pass, unless rejected commands use up their nonce too. The
durability review (F6) showed that the gateway uses a nonce up about a millisecond before
the journal holds it durably, so a crash can un-use it.

**Options considered:**
- *Check at the sequencer:* one place for all state, but a replayed message would cost a
  full verification at a gateway before being rejected, and the sequencer, on every
  command's path, would hold state for every account. Rejected.
- *Use the order id's sequence as the nonce:* cancels and modifies name an existing order,
  and a rejected place doesn't move the engine's sequence. Rejected.
- *Exactly the next nonce, no gaps:* one lost message blocks every later one until it is
  resent. Rejected for INFO.md's "strictly increasing".
- *No bound on the jump:* a client bug that sends `2^64 − 1` locks its own account out for
  good, also across restarts. Rejected for `NonceJump` above 2^32.
- *A window of recent nonces* (as Hyperliquid does): tolerates reordering, but more state
  and rules, and one account's messages already travel one FIFO path. Not needed in v1.
- *Use the nonce up before verifying:* anyone could burn an account's nonces with forged
  messages. Rejected.
- *At the gateway, strictly increasing, used up on forwarding (chosen).*

**Choice and evidence:** PIPELINE.md 6.1 to 6.5, with ten worked attacks (replay; replay
of an engine-rejected place; reordering; another account's order; cross-deployment;
changing the message type; malleability; nonce burning; delayed execution of an abandoned
message; a flood of forgeries, which v1 does not defend against). Unit tests and a property
test of the gateway's nonce state against a small model (PIPELINE.md 18.1, 18.2); the
crash tests check the rebuilt state (18.2, 18.4).

**Trade-offs:** A message the gateway rejected can't be resent once a later nonce has been
used; the client signs it again with a new nonce. `StaleNonce` doesn't answer "did my
message execute?" (the forwarded message may have been another one, or not durable yet);
only a released result does, or after a restart the account's state (PIPELINE.md 12.4).
Nonce state lives only in the journal, so a fresh journal needs a new deployment id
(PIPELINE.md 5.1, 6.3).

**What would change it:** A network path that can reorder one account's messages (then a
nonce window). Several gateways per account (then shared state, or the check moves to the
sequencer). A network gateway (then its replies carry the gateway's start id, and
account-specific rejects go only to the account's own session; D-031).

**Status (2026-09-30):** Built as specified. Evidence: local validation 2026-09-30 (WSL2,
`docs/PIPELINE.md` 22); headline pending PERPSBOX (`docs/RUNBOOK-PERPSBOX.md`).

---

## D-023 Canonical binary encoding of commands and events  (M3, 2026-09-29, extends D-005)

**Decision:** A command is encoded as 40 bytes (a head of tag and small fields, then four
8-byte fields) and an event as 56 bytes, little-endian, field by field, tags from 1. The
same encodings are used in client messages, every ring, the journal and event captures.
Decoding checks every reserved byte and every enum code, so each command has exactly one
encoding. Code in `pipeline/src/codec.rs`; the engine is untouched. Spec:
`docs/PIPELINE.md` section 4.

**Context:** D-005: `repr(C)` records contain uninitialised padding and are not a wire
format, and the workspace denies `unsafe`. The rings carry words (D-024). A signature must
cover one exact byte string per command.

**Options considered:**
- *A separate format per use* (wire, ring, journal): three codecs to test and keep in
  step. Rejected.
- *serde with bincode or similar:* a dependency, and a format we don't specify ourselves.
  Rejected.
- *The codec inside the engine crate:* the engine would grow code it doesn't use, after M2
  closed. Rejected for `pipeline`.
- *One canonical encoding (chosen).*

**Choice and evidence:** PIPELINE.md 4.2 and 4.3 (the tables), 4.4 (canonical form). A
property test: any byte string either fails to decode or re-encodes to itself. A unit test
pins every enum code.

**Trade-offs:** Every command takes 40 bytes, even a cancel that needs about a dozen;
fixed sizes are simpler than the saving. Enum codes follow the engine's declaration order,
so reordering an engine enum breaks a test, on purpose.

**What would change it:** A command or event field that doesn't fit the layouts.

**Status (2026-09-30):** Built as specified. Evidence: local validation 2026-09-30 (WSL2,
`docs/PIPELINE.md` 22); headline pending PERPSBOX (`docs/RUNBOOK-PERPSBOX.md`).

---

## D-024 Rings: our own SPSC ring of atomic words; reject at the edge, wait inside, stop in data-flow order  (M3, 2026-09-29, no queue crate decided by me; revised after review)

**Decision:** Every queue between threads is a bounded single-producer, single-consumer
ring of our own: slots of 64-byte-aligned lines of eight `AtomicU64`, `u64` head and tail
counters each on its own 128-byte line, Release/Acquire publication, each side caching
the other's counter, every page touched at creation, and a `closed` flag that the producer
sets when it is dropped. Capacities are fixed per ring (PIPELINE.md 2.3). When a ring is
full, the edges reject (the sender drops the message; a gateway rejects `Busy` before
verifying, keeping the last 64 lane slots for cancels) and the inside waits (the sequencer
stops taking input, the core waits in its event sink). Operator items that don't fit wait
in the sender, in order, without holding up client sends. The pipeline stops by closing
its rings in data-flow order. Spec: `docs/PIPELINE.md` 2.3 to 2.5, 2.8 and section 3.

**Context:** INFO.md 5: lock-free SPSC rings, no mutex on the hot path, bounded queues,
reject when the core falls behind, never buffer without a limit. My decision: no crate for
queues, and safe Rust.

**Options considered:**
- *`rtrb` or crossbeam* (INFO.md's suggestions): well tested, but new dependencies, with
  `unsafe` inside. Rejected (my decision).
- *A generic ring of `T` over `UnsafeCell`:* stores records as they are, but needs
  `unsafe`. Rejected.
- *`std::sync::mpsc`:* general-purpose (many producers, blocking with thread parking),
  more work per message than a one-producer ring, and no control of memory layout.
  Rejected.
- *Atomic words (chosen):* no `unsafe`; records are encoded into words, which the journal
  needs anyway (D-023).
- Full-ring policy: *wait everywhere* (the open-loop sender would fall behind its schedule
  and hide the delay; no explicit rejects) and *drop everywhere* (a gap in the journal, or
  lost events). Both rejected for the edge/inside split. *The sender waits for the operator
  ring* (the draft): a full operator ring, which happens whenever the sequencer stops, would
  delay every later client send. Rejected for pending operator items.
- Stopping: *one stop flag seen by every thread* (a thread could exit while the thread
  upstream is still producing, losing its last records). Rejected for closing rings in
  data-flow order.
- *One `Busy` rule for every command:* after a stall, queued IOCs would trade against
  quotes whose cancels were refused. Rejected for 64 slots of headroom for cancels.

**Choice and evidence:** PIPELINE.md 3.1 and 3.2 (the memory-ordering argument, including
closing and the counters main reads), 2.5 (capacities from INFO.md 5's formula), 2.8 (the
shutdown order), 18.3 (a 10-million-record concurrency test, also with a slow consumer and
a close at the end). The cost of one ring hop is a microbenchmark (`pipeline_parts`).

**Trade-offs:** Concurrency code of our own (estimated at about 200 lines). Every record
is copied word by word into and out of each ring. Capacities rest on an assumed p99.9
fsync of 10 ms until the fsync probe measures the disk. Places and modifies are refused 64
slots earlier than cancels.

**What would change it:** The ring hop showing in the core's profile (then larger batches).
A disk slower than assumed (then larger journal and event rings).

**Status (2026-09-30):** Built as specified. After the build's review, the gate releases a
command's events only with its trailer, unless the command alone fills the event ring
(PIPELINE.md 2.5, 12.1). Evidence: local validation 2026-09-30 (WSL2, `docs/PIPELINE.md`
22); headline pending PERPSBOX (`docs/RUNBOOK-PERPSBOX.md`).

---

## D-025 Journal: segment files, CRC32C per record, group commit, output gating  (M3, 2026-09-29, output gating decided by me; revised after review)

**Decision:** The sequencer sends every command to a journal writer thread. It appends
80- or 152-byte records (field by field, a CRC32C on each, computed by slicing by 8) to
1 GiB segment files filled with zeros in advance, and makes them durable with one `pwrite`
and one `fdatasync` per batch: when the batch holds 4,096 records (never more), or when its
oldest record was sequenced `T` = 1 ms ago. After each `fdatasync` it publishes a durable
watermark. The gate releases a command's events only when its seq is at or below the
watermark; the core runs ahead, bounded by the rings. Any I/O error, like any panic,
aborts the process. Signed records keep their nonce, expiry and signature (PIPELINE.md
Q3). Crash recovery and restart: D-030. Spec: `docs/PIPELINE.md` sections 11 and 12.

**Context:** INFO.md 5: group commit about once a millisecond; results released only after
their batch is durable (my decision: output gating); the backlog bounded by the buffers.
D-011: the journal header records the hash seed.

**Options considered:**
- *fsync per order:* durable per order, but at most `1/F` orders a second. Kept only as
  the "fsync per order" ablation.
- *Flush as soon as the previous flush is done (`T` = 0):* the same batching and lower
  latency (`F` to `2F` instead of about `T/2 + F`), but up to `1/F` fsyncs a second.
  Measured against `T` = 1 ms by the `T` sweep and as a third arm of the ablation; the
  evidence decides `T`.
- *Release results before they are durable:* lower latency, but a crash could undo
  something a client saw. Rejected (my decision).
- *Make the core wait for each batch:* the core would idle about 1 ms per batch. Rejected:
  the core runs ahead, bounded by the rings.
- *`fsync`* also flushes timestamps; *`fdatasync`* chosen.
- *`fallocate`d segments:* the first write into each reserved block is a metadata change
  that `fdatasync` must also flush. Rejected for writing zeros in advance.
- *`O_DIRECT`:* alignment rules, and replay couldn't use the page cache. Rejected.
- *One checksum per batch:* a torn batch would lose every record in it. Rejected for one
  per record. *CRC32 (zlib)* is equivalent; CRC32C chosen, as storage systems use it.
  *Byte at a time:* about 6 to 7 cycles a byte, a third of the writer's core at 2M
  records a second. Rejected for slicing by 8.
- *Retry an `fdatasync` that failed:* unsafe on Linux, which reports the error once and
  marks the pages clean. Rejected: abort, and let recovery re-write (D-030).
- Flush timer: *started when the writer takes the first record:* a record that waited
  during a slow flush would wait `T` more. Rejected for the record's own sequencing time.

**Choice and evidence:** PIPELINE.md 11.2 and 11.3 (layouts, with two worked records and
their CRCs, recomputed after the format change), 11.5 (the rule and its cost: on average
`T/2 + F`, at most `T + F`), 12.4 (what a client sees after a crash). Evidence for `T`:
the `T` sweep including `T` = 0 (15.8) and the "fsync per order" ablation, in
BENCHMARKS.md M3.

**Trade-offs:** Every client result waits for its batch, on average `T/2 + F`. The journal
holds the hash seed, so it must be protected like the seed. An I/O error stops the
exchange. Preallocation takes seconds before large runs, and does nothing on copy-on-write
file systems. "Durable" means "as reported by `fdatasync` on the file system in use": M3
doesn't cut the power, and some setups make `fdatasync` a no-op, which the probe refuses or
flags (PIPELINE.md 11.6, 15.10).

**What would change it:** A hot standby (durability becomes "the standby has it", INFO.md
12.3). A disk with slow or erratic `fdatasync` (a longer `T`, or a better disk:
PIPELINE.md Q2). The `T` sweep showing a clearly better interval.

**Status (2026-09-30):** The owner's answers of 2026-09-30: probe the disk first, and use
a box with local NVMe for the durable rows if the `fdatasync` p99 is over 5 ms (Q2);
signatures and expiry journaled (Q3). `T` stays 1 ms until the `T` sweep on PERPSBOX.
Evidence: local validation 2026-09-30 (WSL2, `docs/PIPELINE.md` 22); headline pending
PERPSBOX (`docs/RUNBOOK-PERPSBOX.md`).

---

## D-026 Thread layout and pinning  (M3, 2026-09-29, pinning through libc decided by me; revised after review)

**Decision:** One sender, `N` gateways, one sequencer, one journal writer, one core and one
gate. Every thread busy-polls and is pinned to one logical CPU by one small `unsafe`
function (`sched_setaffinity` through `libc` 0.2.189). The core and the sequencer get
whole physical cores (their SMT siblings stay idle), chosen as the quietest by a jitter
probe; gateways get one physical core each. The gate and the journal writer share a
physical core in signed mode and get one each in pre-verified mode. On PERPSBOX: 10
gateways (up to 19 with `--gateway-smt`), at most `floor(CPU quota) − 2` spinning threads,
all on one CPU package, and any run the kernel throttled is discarded. Locally: 2 gateways
on CPUs 0 to 6, with CPU 7 left to the recorders. Spec: `docs/PIPELINE.md` 2.2 and 2.6.

**Context:** INFO.md 3 and 5: a pinned, busy-polling core thread; parallel gateways.
INFO.md 5a: headline runs on PERPSBOX; locally, the recorders keep their own CPU. My
decision: no crate for pinning.

**Options considered:**
- *The `core_affinity` crate* (INFO.md's suggestion): a dependency for one system call.
  Rejected (my decision).
- *No pinning:* the scheduler moves threads between CPUs, and the tail latency pays for
  it. Rejected.
- *`taskset` on each thread id:* no `unsafe`, but depends on an external tool. Rejected.
- *Sleep when idle:* saves CPU, but waking takes tens of microseconds, which lands in the
  tail. Rejected for spinning (tests use "spin then yield").
- *Gateways on SMT siblings by default:* more threads, but elliptic-curve arithmetic
  shares the core's multiply units. Made an option, measured by the verify-scaling probe.
- *The gate and the writer always sharing a core* (the draft): in pre-verified mode with
  the journal discarded, the writer never blocks, and the two together need more CPU per
  command than the core, so the core-path search could measure their core instead.
  Rejected for a core each in that mode.

**Choice and evidence:** The layout tables of PIPELINE.md 2.6, derived from sysfs
topology, with checks against the cgroup CPU quota and its throttling counters. The
verify-scaling probe (15.10) is the evidence for the gateway count and for SMT; each
thread's busy share and the "limited by" label (15.4) show which thread limited each
result; the layout is recorded with every number in BENCHMARKS.md.

**Trade-offs:** Spinning threads use whole CPUs even when idle, and each counts against
the container's quota. A container has no core isolation: other work can still land on
the idle siblings. WSL2's virtual CPUs are not pinned to host cores, so local numbers are
for development only.

**What would change it:** A machine with isolated cores (put the core and the sequencer
there). Verify scaling showing SMT siblings worth using by default.

**Status (2026-09-30):** Built as specified; the gateway count waits for the PERPSBOX
probes. Evidence: local validation 2026-09-30 (WSL2, `docs/PIPELINE.md` 22); headline
pending PERPSBOX (`docs/RUNBOOK-PERPSBOX.md`).

---

## D-027 M3 synthetic flow and open-loop load generator (assumed parameters)  (M3, 2026-09-29, extends D-012; revised after review)

**Decision:** One seeded flow over 67 markets whose content doesn't depend on the offered
rate: a fair value per market (a random walk of ±3 ticks every 15 ms of flow time, with a
2% to 6% jump with probability 1/15,000 per step); 4 market makers per market quoting 3
post-only levels per side and requoting (modify, or cancel and place) on moves, pulling and
requoting on jumps; 2,000 takers sending IOCs; 402 high-leverage accounts meant to be
liquidated on jumps; 200 accounts holding 1,000 long-lived orders; `SetMark` every 100 ms
(fair value ± 2 ticks) and at once after a jump; one withdrawal a second; an insurance
fund capitalised with only $1,000, so that the shortfall path runs. Setup (markets,
deposits, leverage, first quotes, first positions) goes through the journal. Messages are
pre-signed with `expires_at = u64::MAX`; pre-verified items are kept in a compact 64-byte
form; send times are a Poisson process at the offered client-command rate; the sender never
waits, for replies or for ring space. Spec: `docs/PIPELINE.md` section 14.

**Context:** INFO.md 7: synthetic flow on 67+ markets with one fair-value path per
market, market makers, takers, a thin long-lived layer, cohorts, setup through the journal
only, pre-signed messages, two injection modes; the parameters are assumptions, and
calibration is "Later". INFO.md 8: open loop. INFO.md's M3 row: liquidation count, fund
drawdown and peak shortfall from the run.

**Options considered:**
- *Extend the M1 deep flow:* one market with a fixed mid. Rejected.
- *Content tied to wall time* (the flow's clock equal to the send clock at every rate):
  runs at different rates would apply different commands. Rejected for rate-independent
  content, with Poisson send times on top.
- *Evenly spaced sends:* flatters the tail. Rejected for Poisson (kept for debugging).
- *Maker quotes of taker size, as in D-012:* fills make later modifies fail (D-012 lost
  5.3% of its commands to rejects). Rejected for quotes 10 to 1,000 times a taker's size,
  replaced often.
- *Makers keep modifying through a jump:* every stale quote would cross or be swept.
  Rejected for pulling and requoting.
- *Signing while sending:* tens of microseconds per message would cap the offered rate.
  Rejected for pre-signing.
- *A $100,000 fund:* the model gives the fund about −$400 over the headline run, so the
  drawdown would be tiny and the shortfall always zero, never exercising
  `InsuranceShortfall`. Rejected for $1,000.
- *Building the first high-leverage positions while the mark moves* (INFO.md 7): would add
  a generator mode for an effect limited to the warm-up. Not done; a stated deviation:
  those positions start at the frozen starting mark, and entries spread during the timed
  flow.
- *Interleaving the market makers' messages within a step:* smoother arrivals per gateway,
  but real market makers requote in bursts over one connection. Not done; the bursts are
  kept and their queueing reported.

**Choice and evidence:** A model of the generator gives about 100.5k commands per second
of flow time: of client commands, 32.5% places, 32.5% cancels, 30.9% modifies and 4.05%
IOCs; 670 marks and 0.30 jumps a second; about 2.15 liquidations per jump, about 42 in the
headline run (PIPELINE.md 14.7). Tests pin the stream, the mix (within 1.5 points),
per-account nonces and order sequences, and that every order is inside the band of the
generator's own mark (18.1); the smoke test asserts a liquidation and a shortfall (18.4).
The reject share and the liquidation count are measured in the first M3 run and added here.

**Trade-offs:** Uncalibrated (INFO.md 12.4). Rejects cluster around jumps, and the model
can't predict them. Arrivals are Poisson in aggregate but bursty per gateway, which makes
the signed row harder (PIPELINE.md 17). Different rates measure different stretches of the
flow, and repetitions share the seed (the headline also runs with a second seed).
Pre-signing costs seconds to a minute and 144 bytes of memory per message (0.94 GB for the
100k/s headline run).

**What would change it:** Calibration from recorded Polymarket data. A reject share above
5% (tune and record here, as D-012 did).

**Status (2026-09-30):** Built as specified; the reject share and liquidation count of the
headline run go here after PERPSBOX. Evidence: local validation 2026-09-30 (WSL2,
`docs/PIPELINE.md` 22); headline pending PERPSBOX (`docs/RUNBOOK-PERPSBOX.md`). *Since
2026-09-30 (D-034):* the calibration named above exists as a second flow beside this one,
`--flow polymarket`; this flow, its plan, digest and pinned tests are unchanged, and it
stays the default and the conservative stress flow. Its runs now also record `run.flow =
m3` (older summaries read so), and `--bursts` can give it bursty arrivals.

---

## D-028 Measurement: stamps in records, our own histogram, percentiles over offered load, max rate at a limit  (M3, 2026-09-29, own histogram decided by me; revised after review)

**Decision:** Every stage stamps `Instant` time (nanoseconds since the run started) into
the records it passes on; the gate turns the stamps into per-stage latencies in its own
histograms (log-linear, 128 buckets per power of two, 7,424 buckets, at most 0.79% high).
Latency is measured from the scheduled send time, and commands that were never served
count as infinitely late in the end-to-end percentiles, which are therefore over offered
load. A run is a 5 s warm-up, then a 30 s window chosen by scheduled time, repeated 3
times, interleaved. Every run checks its own conditions (clock source and cost, clock
inversions, CFS throttling, page faults on hot threads, each thread's busy share, the
CPU's frequency) and says which thread limited it. The maximum rate at a latency limit is
found by doubling and three bisections at one run per probe, then three runs at the
boundary; the headline and the durable limit are defined in advance. The core-path search
runs with the journal in discard mode. Spec: `docs/PIPELINE.md` section 15.

**Context:** INFO.md 8: open loop, a histogram at every stage, maximum rate at a latency
limit. INFO.md 5: separate numbers for the core path, verification, and signed order to
durable ack with the fsync time. My decision: no histogram crate.

**Options considered:**
- *The `hdrhistogram` crate:* a dependency. Rejected (my decision); ours uses the same
  idea in a small file.
- *Raw TSC reads:* a few ns cheaper, but they need calibration and checks that the vDSO
  already does. Rejected.
- *Each thread records its own stage:* histogram work and memory on every hot thread.
  Rejected for stamps in records and one recording thread.
- *Latency from the actual send time:* hides the generator's own lateness (coordinated
  omission). Rejected.
- *Percentiles over completed commands only:* at overload points they describe the
  survivors and look fine while most commands are dropped. Rejected for "over offered",
  with "of completed" shown next to it at overload points.
- *One run per point:* run-to-run noise reached 15% locally (BENCHMARKS.md M1). Rejected
  for three.
- *Three runs for every probe of a search:* triples the session for probes far from the
  boundary. Rejected for one run per probe, then three at the boundary.

**Choice and evidence:** PIPELINE.md 15.3: the bucket formula was checked over every
bucket up to 2^40, and percentiles came within 0.24% of exact on random data in a model;
unit tests in 18.1. What measurement costs the core (an estimated 10 to 20% of its time
per command: a clock read, a second cache line of stamps, a trailer slot) is measured once
per session with `--stamps off`. The numbers go to BENCHMARKS.md M3.

**Trade-offs:** Measurement is part of every number, including the maximum rate. A full
session takes about 3 hours of box time. The durable limit is open (PIPELINE.md Q1).

**What would change it:** The `--stamps off` comparison showing more than about 10% (then
move the stamps to a separate ring). A clock that costs more than 50 ns (then stamp every
k-th command). The owner's answer to Q1.

**Status (2026-09-30):** The owner's answer of 2026-09-30: the durable limit is `2 × T`
plus the disk's p99 flush time, the median `fdatasync` p99 of the pre-verified 20k/s runs
(Q1). After the build's review: the writer's busy share leaves out its time in
`fdatasync`, the limit label names the disk only under back-pressure and says "not
saturated" otherwise, medians use valid runs only, and a resumed session reuses a run only
with the same settings (PIPELINE.md 15.4, 15.5, 22). Evidence: local validation 2026-09-30
(WSL2, `docs/PIPELINE.md` 22); headline pending PERPSBOX (`docs/RUNBOOK-PERPSBOX.md`).
*Since 2026-09-30 (D-033, D-034):* a session gives one headline per signing scheme and per
flow with its stress switches, each judged on its own timing runs; the durable limit stays
one per session, always from the M3 flow's pre-verified 20k/s runs, whatever flow a
command sends (PIPELINE.md 15.7).

---

## D-029 Replay and the determinism test  (M3, 2026-09-29, revised after review)

**Decision:** Replay builds a fresh `Engine<Book, Fast>` from the journal alone, with the
engine options and hash seed from its header, after recovery (D-030) and a check that the
journal was written with the same engine semantics (`engine_semantics`), and applies every
record in order. The replay test captures the live run's released events in memory,
replays the journal after the run, and requires: the same record count, identical events
slot for slot, an equal `EngineSnapshot`, and the same again with a different hash seed;
in the smoke test also through `Engine<ReferenceBook, Naive>`. The result is "not checked"
if the capture was incomplete or the journal discarded. A separate audit re-verifies every
journaled signature and checks ownership and per-account nonce order. Spec:
`docs/PIPELINE.md` section 13.

**Context:** INFO.md scorecard row 3; INFO.md 5: "the replay test compares the full engine
state and the event stream against the original run; no hashing library is needed".

**Options considered:**
- *Compare hashes of the state and the events:* needs a hash, and a mismatch doesn't say
  where. Rejected for direct comparison, which reports the first difference.
- *Write the capture to disk during the run:* its I/O would compete with the journal's.
  Rejected for memory, written out afterwards.
- *Compare separate processes through files only:* the snapshot has no file format. Kept
  as a convenience (`snapshot.txt`), not as the test.
- *Verify signatures during replay:* minutes of CPU the state doesn't need. Rejected for a
  separate audit.
- *Trust the byte format alone to tie a journal to an engine:* a later build with changed
  fee rounding would replay the same journal into a different state, silently. Rejected
  for an `engine_semantics` number in the header, pinned by a golden-stream test.
- *Pin the exact commit:* would refuse the restart that follows a poison-pill fix.
  Rejected; the commit and build profile are recorded and printed, not enforced.

**Choice and evidence:** PIPELINE.md 13.1 to 13.5. The smoke test (18.4) runs the replay
test locally on every test run, and the kill test replays after crashes; the headline
run's result goes to BENCHMARKS.md and to REPORT.md's determinism line.

**Trade-offs:** The capture needs about 2.5 GB of memory for the headline run, so it is on
only for a fourth, replay-check run. The test proves that the pipeline adds nothing outside
the journal; it doesn't prove the engine right (the M1 and M2 property tests do that), nor
durability (the crash tests of PIPELINE.md 18.2 and 18.4 do that). The audit proves
authorisation only relative to a registry the auditor trusts from another source.

**What would change it:** A hot standby or state roots (INFO.md 12.3), which replay the
journal continuously and would take over this check.

**Status (2026-09-30):** Built as specified. Evidence: local validation 2026-09-30 (WSL2,
`docs/PIPELINE.md` 22); headline pending PERPSBOX (`docs/RUNBOOK-PERPSBOX.md`).

---

## D-030 Crash recovery and restart  (M3, 2026-09-29, after the durability review)

**Decision:** At start, recovery reads the whole journal. The CRC decides whether some
bytes are a record; a record with a valid CRC that doesn't fit (wrong seq, kind, tag or
mode, decreasing timestamp) is an error, as is a segment header whose identity differs
from segment 0's. The journal ends at the first bytes that are not a record, and only if
every nonzero byte after that lies within what one unfinished flush could have written;
otherwise recovery stops with an error and changes nothing. Recovery then copies the torn
tail aside, zeroes it, re-writes and syncs the kept part of the last segment, and only then
lets anything be derived from the journal. A restart checks its configuration against the
journal's identity, replays the journal (engine, nonce table, next seq, last timestamp),
re-anchors its clock above the last timestamp, and starts a new segment. Any panic aborts
the process. Spec: `docs/PIPELINE.md` 2.8, 11.1, 11.2, 11.7, 11.8 and 13.5.

**Context:** Owner decision: clients see only durable results, so nothing released may be
lost, and nothing unreleased may come back. The durability review (PIPELINE.md 22) found
that the draft's recovery (1) accepted stale records from an earlier life after a second
crash, which could execute a resent signed order twice; (2) trusted bytes that were only in
the page cache after a failed `fsync`; and (3) treated any inconsistency as the end of the
journal and zeroed what followed, silently deleting released results. Rust ends only the
panicking thread by default, which could hang the run or release half a command.

**Options considered:**
- *Keep the longest valid prefix, zero the rest of the last segment and later headers*
  (the draft): stale bodies of later segments survive and are read on after a second
  crash. Rejected.
- *An epoch mixed into every record's CRC* (as RocksDB's recyclable logs put the log
  number in each record): old records fail their CRC with no zeroing, but the epoch must
  be new for every life even when the header that carried it was lost, so it needs a random
  or separately stored epoch. Rejected for zeroing the tail region, which is simpler to
  argue, and needs no new field.
- *Write zeros over every byte after the end, in every segment:* also correct, but it
  writes seconds' worth per GB of preallocated space at every restart. Rejected for
  reading instead, to check that everything outside one flush's tail region is already
  zero (it is, by induction over lives), and writing zeros only over that region.
- *Treat every inconsistency as the end* (the draft): a header written by a restarted
  process, or bit rot in the middle, would roll back released results. Rejected: valid
  data that doesn't fit is an error, for a person to decide.
- *Trust what recovery reads:* after a failed `fsync`, Linux keeps unwritten bytes readable
  but marks them clean. Rejected for re-writing and syncing the kept part.
- *Pad every batch to a 4 KiB boundary*, so a page rewrite can't touch released bytes:
  about 14% more bytes at 100k signed/s. Rejected for the stated assumption that sector
  writes are atomic.
- *`panic = "abort"` in the release profile:* covers release builds only. Rejected for a
  panic hook that aborts in every build.

**Choice and evidence:** A Python model of the recovery rule through 20,000 random runs of
write, crash (power loss keeping a random subset of the unsynced 512-byte sectors),
recover, restart and crash again: no released record lost, no record from an earlier life
accepted; the draft rule, in the same model, accepted stale records. Tests: the recovery
rules and errors (PIPELINE.md 18.1), crash sequences on a simulated disk that also fails
syncs the way Linux does (18.2), and a SIGKILL test that checks the released events are a
prefix of the replayed stream across restarts (18.4).

**Trade-offs:** A restart reads the whole journal (about a second per GB) and re-writes up
to one segment. A mismatch or damage in the middle stops the exchange until a person
decides. Relies on sector writes being atomic. A poison-pill command stops every restart
until the engine is fixed. A fresh journal needs a new deployment id.

**What would change it:** A hot standby (recovery becomes failover, INFO.md 12.3). A device
without atomic sector writes (then pad batches to 4 KiB). Journals far larger than a few
GB (then an index of segment ends, so a restart need not read everything).

**Status (2026-09-30):** Built as specified. After the build's review: nonzero bytes in
both parts of the tail region are an error, a new journal zeroes a torn first flush and
refuses a valid header, torn copies are never overwritten, and every directory on the way
to the journal is fsynced when created (PIPELINE.md 11.1, 11.8, 22). Evidence: local
validation 2026-09-30 (WSL2, `docs/PIPELINE.md` 22); headline pending PERPSBOX
(`docs/RUNBOOK-PERPSBOX.md`).

---

## D-031 Security boundary of v1, and what a network gateway needs first  (M3, 2026-09-29, after the security review)

**Decision:** v1's gateways are built for one trusted, in-process client, the load
generator, and the spec says so. They defend against replays, malleability, cross-domain
reuse, spoofed ownership, nonce burning and delayed execution of old messages. They do not
defend against: floods of forged messages (each costs a verification, and a forgery
doesn't use up its nonce, so the same bytes work again); one funded account resting
millions of orders (no per-account open-order limit, so the book can outgrow its capacity
and one command can cost O(its resting orders)); probing another account's nonce through
the order of reject reasons; key reuse from wallets; and revoking a key without a restart.
Before any client connects over a network, the gateway must add the preconditions below.
Spec: `docs/PIPELINE.md` 6.5 (attacks 9 and 10) and 7.4.

**Context:** INFO.md 12.6: a network gateway and per-account rate limits are "Later". The
security review (PIPELINE.md 22, S1, S3, S9 to S12) found the draft claiming more than it
delivered ("garbage costs nanoseconds"; "safe once a network gateway exists").

**Options considered:**
- *Leave the claims as written:* wrong, and the first thing an interviewer would probe.
  Rejected.
- *Build the defences into v1:* sessions, per-connection accounting, rate limits and an
  engine order cap, for a client that doesn't exist yet. Rejected; listed as
  preconditions instead.
- *A minimum order size at the gateway:* raises an attacker's cost about 100 times but
  bounds nothing. Rejected; the bound belongs in the engine.

**Choice and evidence:** The preconditions, each tied to an attack in PIPELINE.md:
1. Bind each connection to one account at login (one signature per session, or a TLS
   client certificate), and reject a message whose account differs from the session's
   before any other check.
2. Count `BadSignature` per connection and close it after 3; verify round-robin across
   connections; never throttle by the claimed account before the signature verifies
   (that turns a flood into a lockout of the victim); charge work to the connection.
3. Send account-specific rejects (`StaleNonce`, `NonceJump`, `UnknownAccount`) only on the
   account's own session; others get one generic reject.
4. Every reply carries the gateway's start id, and a client treats `StaleNonce` as final
   only from a gateway started after the one it first sent to.
5. Per-account rate limits (INFO.md 12.6); a per-slot open-order cap in the engine (an
   O(1) counter in the place and replace checks, with its own reject reason; an engine
   decision after M3); an order-sequence jump bound, like `NonceJump`, rebuilt from the
   journal.
6. Keys added or revoked while running: a control ring from main to each gateway, each
   change journaled as an operator record so the audit knows which key was valid when.
7. Keys dedicated to this protocol; EIP-712 if wallets ever sign.

**Trade-offs:** v1 can't be exposed to untrusted clients as is. That is the intended scope
of v1 (INFO.md 5: "Gateways are in-process threads in v1").

**What would change it:** A network gateway (then all of the above first). Wallet
clients (then EIP-712, D-021).

**Status (2026-09-30):** Built as specified. Evidence: local validation 2026-09-30 (WSL2,
`docs/PIPELINE.md` 22); headline pending PERPSBOX (`docs/RUNBOOK-PERPSBOX.md`).

---

## D-032 Optional libsecp256k1 verifier  (M3, 2026-09-30, opt-in decided by the owner)

**Decision:** Bitcoin Core's libsecp256k1, through rust-bitcoin's `secp256k1` crate
(`=0.33.1`, feature `std` only, with `secp256k1-sys` 0.14.1, which vendors
libsecp256k1 v0.8.0), is an opt-in second signature verifier next to `k256`. It is
compiled only with the gateway's `c-secp256k1` feature (the bench crate's feature of the
same name turns it on), chosen per run with `e2e --verifier libsecp256k1`, and never the
default. Everything but the verification call stays as it is. Spec: `docs/PIPELINE.md`
5.7. *Since 2026-09-30 (D-033):* the `c-secp256k1` feature also turns on the crate's
`recovery` feature, for the EIP-712 scheme's signer recovery: one more module of the same
reviewed C, no new crate, `Cargo.lock` unchanged (`docs/SUPPLY-CHAIN.md`).

**Context:** D-021 chose `k256` and kept the C library as the fallback "if M3 shows 100k
signed orders/s can't fit, after its own review". Section 17 of PIPELINE.md: the gateways
needed grow with `t_v`, and PERPSBOX's 10 gateway cores carry 100k/s only if `t_v` is at
most about 69 µs; locally `k256` verifies in 79 µs. On 2026-09-30 the owner chose to
measure the two side by side on the rented box rather than wait for the headline to fail,
had it reviewed (the build script, per D-006, and the vendored C against upstream;
`docs/SUPPLY-CHAIN.md`), and approved it as an opt-in. The owner decided that the Rust
wrapper itself need not be swept.

**Options considered:**
- *Wait for the headline to fail, then add it* (D-021 as written): a second session on
  another rental, and no comparison on the same machine. Rejected by the owner.
- *Replace `k256` with it:* the default build would compile C, which D-001 and D-006 keep
  out, and the pure-Rust baseline would be lost. Rejected.
- *An opt-in feature, chosen per run* (chosen): the default build is unchanged and never
  compiles the C; one extra build on the box runs the same flow with the same gateway
  count.
- *A libsecp256k1 context held by each gateway thread, made once at start:* not possible
  through the crate's safe API in 0.33, whose verification call takes no context, so none
  is held (PIPELINE.md 22). What each gateway does hold is its keys, parsed once by the
  chosen library when the registry is loaded (5.4), as with `k256`.
- *The `libsecp256k1` crate:* the abandoned lookalike, banned in `deny.toml`. Not
  considered.

**Choice and evidence:** `gateway/src/verifier.rs`. The same 72 signed bytes, the same
SHA-256 (computed with `k256::sha2` and passed as libsecp256k1's digest), the same checks
before it, our own low-S check first. Cross-check tests: both verifiers accept what `k256`
signs and refuse the same edits, out-of-range scalars, wrong keys and high-S twins, and the
worked example of 5.6 as a fixed vector; every gateway test, the signed pipeline test and a
signed smoke run pass with libsecp256k1 verifying. Locally (WSL2, release): 41.1 µs per
verification against `k256`'s 78.9 µs (1.9 times faster), and the scaling curve roughly
doubles too (PIPELINE.md 22). The PERPSBOX numbers come from the runbook's second pass.

**Trade-offs:** C code on the verification path of the opt-in build: memory-unsafe code
from outside the Rust ecosystem, reviewed as upstream's (the vendored tree matches
libsecp256k1 v0.8.0 once the crate's six shipped patches and its symbol renaming are
applied), with the wrapper's own run-time code, `unsafe` FFI calls included, not swept. A
second build on the box. The crate's `std` feature is on (2026-09-30): one context per
thread, allocated before the timed flow by `verifier::prepare_this_thread`, instead of a
global copy behind a spinlock rebuilt on every call (2.65 µs of each verification; after
the change libsecp256k1 verifies in 39.2 µs against `k256`'s 80.1 µs locally, 2.04x;
PIPELINE.md 22). The audit of a
libsecp256k1 run verifies with libsecp256k1 too, so it is not an independent check by the
other library.

**What would change it:** PERPSBOX's numbers. If libsecp256k1 lets 100k signed orders/s
fit where `k256` doesn't, making it the default for signed runs is the owner's call, and
would come with a sweep of the wrapper and its `std` feature. If the gain is small there,
it stays opt-in, or goes.

**Status (2026-09-30):** Built. Evidence: local validation 2026-09-30 (WSL2,
`docs/PIPELINE.md` 22); headline pending PERPSBOX (`docs/RUNBOOK-PERPSBOX.md`, section 6).

---

## D-033 A second signing scheme matching Polymarket Perps: EIP-712 over keccak-256 of MessagePack, salt and timestamp, signer recovery  (M3 add-on, 2026-09-30, scheme and own keccak decided by the owner)

**Decision:** An opt-in second authentication scheme, chosen per run with
`e2e --auth eip712` (default `perp`, the D-021/D-022 scheme). A message is signed the way
Polymarket Perps signs a trading operation (sources and golden vectors below):

1. Build the operation's compact form, a positional array with absent entries dropped:
   - `PlaceOrder` → `["createOrders", [[iid, buy, p, qty, tif, po, c]]]`, with `ro`
     omitted (v1 has no reduce-only flag) and `tr` omitted (no triggers). As of
     2026-09-30 the SDKs (py-sdk `to_raw_order`, ts-sdk `toRawPerpsOrder`) append two
     more slots when a builder session is set, one reserved and a builder-attribution
     pair `[address, feeRate]`; v1 has no builders, so both are absent and dropped too;
   - `CancelOrder` → `["cancelOrdersCOID", [c]]`;
   - `ModifyOrder` → `["modifyOrdersCOID", [[c, p, qty]]]`, where `qty` is the new *total*
     size, the same convention as ours (D-008).

   `iid` is our market id, `buy` the side, `tif` `"gtc"` or `"ioc"`, `po` the post-only
   flag (always present), `c` the order id as 32 lowercase hex digits, and `p`/`qty` the
   price and size as minimal decimal strings (below).
2. MessagePack-encode it with the smallest encodings (fixarray, fixstr/str8, positive
   fixint/uint8..uint64, `c2`/`c3`), and hash the bytes with keccak-256.
3. EIP-712: `digest = keccak256(0x19 0x01 || domainSeparator || keccak256(OP_TYPEHASH ||
   data || uint256(salt) || uint256(ts)))`, with `Op(bytes32 data,uint64 salt,uint64 ts)`
   and the domain `EIP712Domain(string name,string version,uint256 chainId)` =
   `{"Polymarket", "1", deployment}`. Our deployment id is the chain id, so a signature is
   tied to one deployment; Polymarket's production value is 137.
4. The client signs the digest with secp256k1 (RFC 6979, low-S) and sends `r || s` and the
   recovery id `v` (0 or 1 in our message, byte 6; Ethereum writes it as 27 or 28).

The gateway rebuilds the compact form from the command it received, computes the digest,
**recovers** the signer's public key from `(digest, r, s, v)`, derives its Ethereum
address (the last 20 bytes of keccak-256 of the uncompressed key) and compares it with the
account's registered address. Replay protection replaces the nonce: `ts` (Unix ms) must be
at most 5 minutes old and at most 60 s ahead of the gateway's clock, and the request
`(account, salt, ts, market)` must not have been accepted before (the market added by the
owner on 2026-09-30, "Trade-offs"). The gateway remembers accepted
requests in a fixed-size table, allocated and pre-touched before the timed flow. keccak-256
and the MessagePack encoder are our own code; recovery uses `k256` (`recover_from_prehash`,
already in the locked `ecdsa` 0.17.0) or, in the `c-secp256k1` build, libsecp256k1's
recovery module. Spec: `docs/PIPELINE.md` 5.8.

**Context:** On 2026-09-30 the owner asked whether our engine reaches its signed rate under
Polymarket's own design. Polymarket's docs, OpenAPI spec and SDKs (checked by two research
passes, every claim re-fetched; 88 of 89 and 28 of 28 confirmed) show that every Perps
order, cancel and modify carries its own EIP-712 signature by a delegated "proxy" key,
over keccak-256 of the MessagePack-encoded compact operation, with a random salt and a
millisecond timestamp. Signatures older than 5 minutes are refused, a reused request is
refused (`signature_already_used`), and the server recovers the signer rather than reading
an account from the request. Their py-sdk carries golden vectors generated from their
TypeScript SDK (`tests/unit/test_perps_signing_golden.py`). The scheme is very close to
Hyperliquid's (MessagePack, keccak, EIP-712, an approved agent key).

**Options considered:**
- keccak-256: *write our own* (Keccak-f[1600], about 120 lines, tested against official
  known answers; chosen by the owner, as for the rings and the histogram, D-024/D-028);
  *RustCrypto `sha3`* (same family as `k256`, but two new crates through D-006; rejected).
- MessagePack: *a crate such as `rmp`* (allocates, and more code to review than the few
  shapes we sign; rejected); *our own encoder into a stack buffer* (chosen).
- What the gateway hashes: *the client's own bytes*, as Polymarket's server does with the
  JSON strings it receives (our in-process wire carries parsed fields, not text; rejected);
  *a canonical rebuild from the 40-byte command* (chosen: the audit rebuilds the same bytes
  from the journal, 13.4). So signatures from a real SDK verify here only when the client
  wrote prices and sizes in our canonical form (`"0.5"`, not `"0.50"`).
- Identity: *recover and look the address up* (Polymarket's way); *recover and compare
  with the claimed account's address* (chosen: the account still routes the message to its
  gateway, and the cost is the same recovery); *verify against the registered key* (cheaper
  by one square root, but not what Polymarket's server does; kept for the audit, which has
  no `v`).
- Replay window: *key by salt alone* (random 32-bit salts from a busy client collide within
  5 minutes by the birthday bound; rejected); *key by `(account, salt, ts)`* (chosen first:
  the same signed request; but a copy of a cancel or a modify with another market, which
  is not signed, then used up the genuine request); *key by the digest* (the same effect,
  more bytes per entry, since the digest doesn't cover that market either); *key by
  `(account, salt, ts, market)`* (the owner's choice, 2026-09-30, after the review: such a
  copy is then another request, "Trade-offs").
- Salt source in the load generator: *random, as the SDKs do*; *the plan's per-account
  nonce* (chosen: unique, deterministic, and the flow and its pinned tests stay unchanged).

**Choice and evidence:** Known answers before any pipeline code: keccak-256 of `""` and
`"abc"`; the EIP-712 specification's Mail example (its domain separator hashes 160 bytes,
two keccak blocks); all 12 Polymarket golden vectors, both with `k256` and with
libsecp256k1: each operation's `data` (which pins its MessagePack bytes), each RFC 6979
signature byte for byte (which pins the digest: the file doesn't print digests), and the
address recovered from each, which the file doesn't print either (Circle's `cctp-go` pairs
it with the same test key, and the test derives it from the key). Prices and sizes:
`PRICE_DECIMALS = 2` and `QTY_DECIMALS = 4` for every market of the synthetic flow (they
add up to 6, so one tick times one lot is one micro, D-004), written in minimal form: no
trailing zeros, no trailing point.

**Trade-offs:**
- Cost per message: MessagePack encoding and four keccak-256 blocks, plus recovery instead
  of verification (one extra square root). Measured by the probes as
  `<verifier>.recover_ns` and `eip712.digest_ns`. Locally, our keccak-256 takes 0.41 µs a
  block, so the four take about 1.7 µs; the whole signer check takes 90.7 µs with `k256`
  against 78.5 µs to verify a perp message, and 46.7 µs with libsecp256k1 against
  39.3 µs.
- Memory: the replay table holds every request accepted in the last 5 minutes, in 24-byte
  slots of which at most half are used (the market's 2 bytes fit in what was padding, so a
  slot stayed at 24 bytes when the market was added): 48 to 96 bytes per request it has
  room for (1.4 to 2.9 GB per 30M requests across all gateways; the 100k/s headline's
  tables take 48 MiB per gateway), against 8 bytes of nonce per account.
- In Polymarket's forms, cancel and modify name only the order. Our engine routes them by
  `market`, which is therefore carried but not signed, and a copy of a signed cancel or
  modify with another market passes every gateway check. **The owner chose the replay key
  (2026-09-30):** the market is part of the request, `(account, salt, ts, market)`, for
  every command (places too, for one rule; a place's market is signed, so a copy of a
  place with another market is `WrongSigner` and takes no slot). So such a copy is another
  request: the gateway accepts it, and the engine rejects it and changes nothing
  (`UnknownMarket` for a market that doesn't exist, `UnknownOrder` for one that does,
  because an order id's sequence is unique per account across all markets: `Duplicate`,
  `engine/src/engine/orders.rs`). The genuine message is still accepted, before or after
  the copy, and a copy with the same market is `ReusedRequest`. The cost: whoever holds
  the signed bytes of a cancel or a modify can make the engine reject up to one junk copy
  per market id (the other 65,535 values of a `u16`, while the `ts` is in the window), each
  costing a recovery, a table slot, a lane slot and a journal record, so a flood of them
  could fill a table sized for the honest requests only (`SaltTableFull`); only a network
  gateway's per-session limits would bound that (PIPELINE.md 7.4). They can never make
  the genuine message `ReusedRequest`, and never reach another order. Before this choice
  the key was `(account, salt, ts)`, and one copy sent before the genuine message used up
  its request, so the genuine one got `ReusedRequest` (PIPELINE.md 22, R2). The audit keys
  its uniqueness check the same way, so a journal with copies next to the genuine record
  passes and an exact repeat fails; it still can't see a market changed in the journal
  after the fact, whose replay leaves that order on the book (13.4). The other fix, the
  engine finding a cancel's or a modify's order by its id alone, would close both, at the
  cost of an engine change. v1's one client is in process (PIPELINE.md 7.4), so nobody
  else holds its signed bytes.
- The recovery id `v` is not journaled (kind-1 records keep their 152 bytes, with the salt
  in the nonce word and `ts` in the expiry word). The audit verifies `r || s` against the
  account's registered key over the rebuilt digest, which proves the same thing, for
  every field the digest covers (all but a cancel's or a modify's market, above).
- `engine::id_hash` became public, so that the gateways' salt tables reuse the engine's
  seeded hash (D-011) instead of a second copy of it: a wider engine API, though nothing
  the engine does changes (the owner to confirm).
- Not in this version: JSON/HTTP parsing, batches and TP/SL groups (a message carries one
  command). Refused by `RunConfig::check`: `--resume` after a crash (the gateways would
  need the last 5 minutes of requests from the journal), the verify-on-core ablation,
  pre-verified mode, and a timed flow over 3 minutes (the load generator signs before the
  run, and the gateways refuse a message 5 minutes after its `ts`).

**What would change it:** A golden-vector mismatch (the scheme is wrong somewhere: fix
before any number is reported). Evidence that Polymarket's server keys replays
differently, or accepts high-S or `v` of 0/1 (align the checks). Wanting real SDK
signatures to verify byte for byte whatever their decimal formatting (then carry the
client's strings, not just the parsed command).

**Status (2026-09-30):** Built, opt-in (`e2e --auth eip712`); spec `docs/PIPELINE.md`
5.8, build log in its section 22. Evidence, local (WSL2, Ryzen 7 2700X, built and tested
in the `dev` container only; PIPELINE.md 18.1, 22):
- **Known answers:** keccak-256 of `""` and `"abc"` (go-ethereum); the Keccak team's two
  worked permutations, and NIST's SHA3-256 answers at 0, 1, 135 and 136 bytes through the
  same sponge with SHA-3's padding (XKCP); the EIP-712 Mail example in full, its `v` = 28
  signature byte for byte; the TypeScript SDK's two `data` vectors; and all 12 golden
  vectors as above, with both libraries.
- **Tests:** the whole workspace passes in the default build. With the `c-secp256k1`
  features every test of the scheme passes; the only failures seen there were intermittent
  ones in unchanged perp code, each passing when run alone (PIPELINE.md 22; none in the
  full runs after the review). `gateway` has 125 unit tests (131 with the feature, the
  cross-check of both libraries' recovery over 240 cases among them) and two new
  integration tests (the scheme end to end with real gateways, pipeline, journal and
  audit; no allocation over 1,200 checks). After the replay key gained the market
  (2026-09-30), 130 unit tests (136 with the feature), all passing in both builds. The
  EIP-712 smoke runs, with `k256` and with libsecp256k1: no allocation and no minor fault on any hot thread in the window, the
  replay identical, the audit clean over every forwarded message, and no harness message
  refused.
- **Local numbers** (release, after the review's keccak fix, PIPELINE.md 22, R4): the
  gateway's whole signer check 90.7 µs with `k256` and 46.7 µs with libsecp256k1
  (verification: 78.5 and 39.3 µs); the digest 1.3 µs; one keccak-256 block 0.41 µs; a
  salt-table lookup and insert 25 ns; recoverable signing 71.2 µs. The quick probe,
  `k256`: 10,913, 20,350, 36,823 and 61,510 signer checks a second on 1, 2, 4 and 7 threads
  (the last with SMT), against 12,686, 25,253, 47,116 and 70,540 verifications. By section
  17's formula, 100k/s needs 14 gateways with `k256` and 7 with libsecp256k1 at these
  costs.
- Headline pending PERPSBOX (`docs/RUNBOOK-PERPSBOX.md`, section 7).

**Corrected on building** (in the text above; the decision itself is unchanged except the
replay key, which the owner widened to include the market, below):
- Step 4: the recovery id travels as 0 or 1 in our message; 27 and 28 are refused
  (`Malformed`).
- Choice and evidence: the golden file gives each operation's `data` and signature, not its
  MessagePack bytes, digest or the key's address; the text now says how each is shown.
- Trade-offs, cost: the draft said MessagePack and four keccak-256 blocks cost "under a
  microsecond". Our keccak-256 first took 3.0 µs a block, so about 12 µs, because of how
  its loops were written; with loops over `x` and `y` (the review, PIPELINE.md 22, R4) it
  takes 0.41 µs, so about 1.7 µs. The measured costs are in the text.
- Trade-offs, memory: a request takes 48 to 96 bytes of table (two to four 24-byte slots),
  not "about 24 bytes"; 1.4 to 2.9 GB per 30M requests, not "about 1.5 GB".
- Trade-offs, not in this version: `RunConfig::check` refuses what it lists and, added while
  building, pre-verified mode and a timed flow over 3 minutes; JSON, batches and TP/SL
  groups aren't refused by it, they are simply not built.

**Readings taken while building** (PIPELINE.md 22): each gateway's table is sized for
exactly the messages the sender offers it; the load generator signs each arena with one
`ts`, the wall clock when signing starts, and the harness signs again before a run the
messages wouldn't last through, and refuses the run if even the newly signed ones
wouldn't (signing took too long); the audit allows 10 s of sequencing slack below the
window (`SEQUENCING_SLACK_MS`: the owner to confirm); an EIP-712 arena's `presigned.bin`
has its own magic (`PERPSGN2`) and 8 more header bytes for its signing time, while a perp
arena's file is unchanged.

**Reviewed after building** (2026-09-30; PIPELINE.md 22, R1 to R13; the decision itself is
unchanged). Fixed: the keccak-256 permutation's loops (7 times faster, so the costs above
dropped by about 11 µs a check); the harness's freshness check of newly signed messages;
one headline per scheme in a session's report; the perp arena's `presigned.bin` restored
byte for byte; and smaller points of code and wording. Found: a cancel's or a modify's
market is not signed, so a copy with another market could use up the request, and the
audit couldn't see it (R2, R3). **The owner chose the replay key, 2026-09-30:** the market
is now part of the request, so such a copy is another request, which the engine rejects,
and the genuine message is still accepted; the audit's uniqueness check uses the same key
("Trade-offs"; PIPELINE.md 22, the entry after the review). Still open for the owner: the
engine change that would close the rest (the junk copies, and a market changed in the
journal after the fact), and `engine::id_hash` being public.

---

## D-034 A Polymarket-shaped flow, calibrated from recorded data, and three stress switches  (M3 add-on, 2026-09-30, timing and scope decided by the owner)

**Decision:** A second synthetic flow, `e2e --flow polymarket` (default `m3`, the D-027 flow,
unchanged), whose shape comes from ~22 hours of Polymarket Perps public data we recorded
(D-007), plus three stress switches that can be added to it: `--makers K` (market-making
concentrated in K accounts), `--bursts median|busiest` (a bursty send schedule) and
`--shock real|stress` (correlated multi-market shocks; `stress` adds a liquidation
cascade). The M3 flow stays the conservative stress flow; the new one answers "what does
Polymarket's own traffic shape cost this engine, at 100× its volume". Spec:
`docs/PIPELINE.md` 14.12.

**Context:** A comparison of the M3 flow with the recordings (2026-09-30, 6 agents, every
claim re-checked) found the M3 flow does *more* costly work than Polymarket's (4% takers
vs 0.07%; ~8 cancels per fill vs at least 537; frequent 2–6% jumps), but looks nothing like
it (0.3 bp spread and $110k per level vs ~4.5 bp and ~$10k; 67 identical markets vs 88
concentrated ones; flat $1–3k takers vs a heavy tail with 14% $10–12 dust), and never tests
three things that could break the signed headline or its tail: one busy account (each
account verifies on one gateway, ~25k/s with k256), bursts (Poisson arrivals are smooth),
and correlated shocks (71 markets moved together at 2026-09-30 12:30:07 UTC). The owner
asked for it before deciding whether libsecp256k1 (800k/s signed ceiling vs k256's 400k/s)
should become the default verifier.

**Calibration** (session scratchpad `calib/`: `instruments.json`, `book.json`,
`flow.json`, each re-checked by an independent script on hold-out hours; the corrected
values are the ones used). Generated into a committed Rust table
(`loadgen/src/market_flow/polymarket_profile.rs`) by a standard-library Python tool under
`tools/calibrate/`, which re-derives most of it from the recordings and copies the
costliest fits from the first calibration (`tools/calibrate/inputs/`). Data stays out of
git; the table holds only derived parameters, like any other constant:
- **Instruments:** all 88: price and quantity decimals (their sum is always 6, D-004),
  max leverage (10x on 63, 5x on 9, 20x on 8, 3x on 4, 50x on 4), the real tier tables
  (1–8 rows, 12 distinct), the start price (the recorded mark at 2026-09-30 10:00:00 UTC)
  and the real price grid (5 significant figures: `10^max(0, digits(price_ticks) − 5)`
  engine ticks; 1 for 76 markets, 10 for 11, 100 for XRP).
- **Activity across markets:** each market's own recorded share of maker messages (level
  changes) and of taker orders. Across markets, a lognormal over ln(share) with sigma 0.49
  fits the makers' (flat: top-10 markets 23.4%), and one with sigma ≈ 1.35 the takers'
  (top-10 52.6%, ETH 8.5%).
- **Book shape, per class** (majors BTC/ETH/SOL; alt crypto; long-tail crypto; tradfi
  equities; tradfi macro): spread as a split lognormal in bps (alt 5.14/0.85/0.25,
  long-tail 5.30/0.62/0.95, equities 4.17/0.66/0.34; one real tick for the tick-bound
  markets, the majors and SP500, NAS100 and GOLD; SILVER, WTIOIL and BRENTOIL each their
  own), at least one real tick; 20 levels per side; gaps between levels from the per-class
  shares of 1 to 10 real ticks (after levels 1–4, 5–9 and 10–19), `10 + exp(N(mu, sd))`
  ticks beyond, counted on the sides that show all 20 levels; sizes level by level (level 1
  thin, a median of $1k to $3.7k; the clips at levels 3 to 10): clips from a per-class menu
  (crypto $6,250; tradfi $25k, $50k, $100k; ±3% jitter) plus a background of that level's
  recorded sizes (64 quantiles); dust ($10 to $12.50 in the books, drawn from [$10.50,
  $11.60)) on 1.3% (macro) to 13.7% (long-tail) of a class's levels, mostly at levels
  11–20.
- **Maker activity:** level changes are 36.6% add, 36.6% remove, 13.4% size up, 13.4% size
  down → places, cancels and total-size modifies; 52% at levels 1–5, 27.7% at 6–10, 20.3%
  at 11–20.
- **Takers:** 0.07% of messages (all IOCs: the high-leverage cohort's adds take 0.01 of
  the 0.07 points, "Corrected after review"); notional per taker order = 14.6% dust U[$10.5, $11.6),
  10.7% round-number point masses ($1,000 6.65%, …), else a two-lognormal mixture
  truncated at $12.5 (w 0.669: mu 5.714, sd 2.006; w 0.331: mu 6.694, sd 1.046); capped at
  the market's max notional; market chosen by taker weight; 51% buys; millisecond clusters
  (0.451 starts/s at real volume, size P(1) 0.84, gap p50 7 ms, 62% same market, 90% same
  side), one account per cluster. Priced to sweep: an IOC at the band edge, so fills per
  taker come from real depth.
- **Price dynamics:** per max-leverage class, 1-s fair-value moves from a 3-component
  normal scale mixture in units of each market's own RMS move (e.g. 10x: w 0.63/0.34/0.027,
  sd 0.47/1.17/3.88), no move in 62–74% of seconds, snapped to the real grid; persistent
  jumps over 50 bps 0.051 per market-hour (one per 70,400 market-seconds), of 50 to 123
  bps; the operator's mark every 200 ms per market.
- **Scale:** ratios per message are Polymarket's; volume is not (the real venue sends
  ~750–1,700 messages/s). One flow-second of this flow holds a fixed 100,000 client
  messages (99,997 measured), whatever the offered rate, so at 100k/s it is Polymarket's
  shape at ~100× its volume, with its real price movement per second; at higher rates its
  prices move faster, as the M3 flow's do.

**Stress switches:**
- `--makers K`: K market-making accounts, 1 to 20, quote every market (default: 3 per
  market, spread over 60 accounts; the draft said 12, "Corrected after review"). With K = 3
  each account sends ~⅓ of all traffic, so its one gateway, not the gateway count, sets the
  signed ceiling. The run reports the busiest account's rate and its gateway's load, and
  the search finds the per-account ceiling.
- `--bursts median|busiest`: the send schedule becomes a Cox process: the Poisson rate is
  multiplied, once per second, by `exp(fast + slow)`, two AR(1) processes fitted to the
  recorded per-second message rate (median hour: fast phi 0.358, innovation sd 0.364; slow
  phi 0.99788, sd 0.0129; busiest hour: 0.251/0.218 and 0.99913/0.0054), normalised over
  each phase (its multipliers divided so that they average 1 over the phase's expected
  length, `n / R`), so the phase offers `R` on average and ends on time. Only the schedule
  changes, never the plan (14.1).
- `--shock real|stress`: every 10 s of flow time, N markets (Pareto α 1.41, clamped to at
  least 14 and at most all 88; median 14, 90th percentile 38) move the same way within 52
  ms. `real`: the recorded sizes, 4 to 8 of each mover's own RMS of nonzero 1-s moves
  (median about 5, ~11 bps), each mover keeping the shared direction with 95% chance. `stress`: 2–6%, all one way, with a cascade cohort of
  8 high-leverage accounts per market whose close entries one mark liquidates together.
  The report gives the longest `SetMark` core service time and the most events per
  command.

**Options considered:**
- *Replay the recordings directly* (INFO 12.4): 1-s snapshots and no accounts, and ~1,000
  messages/s; a sped-up replay would move prices 100× faster than reality. Rejected for
  now; the calibrated generator keeps real price speed at benchmark volume.
- *Tune the M3 flow's parameters*: it would lose the conservative stress case. Rejected:
  two flows, both reported.
- *Bursts inside the plan*: would tie the plan to the rate. Rejected: bursts live in the
  schedule, like Poisson arrivals today.

**Trade-offs:** Only public, aggregated data: real account structure is unknown, so maker
concentration is a switch, not a calibrated fact. One day of data: one US open, one macro
event, no weekend. The book-change rates are 1-s lower bounds. The EIP-712 scheme (D-033)
formats prices with its fixed decimals in every flow, not each market's real ones; the
signature cost does not depend on the digits. More memory: 88 books from half to twice
their start prices, 85.6 MB of levels against the M3 flow's 110 MB, but 249 MB of engine
memory in all against 231 MB, since the rest of the reservation is per market (about 1.9
MB each). A fixed count per flow-second means prices move faster than recorded above an
offered 100k/s. The default flow almost never liquidates (below), as on Polymarket; the
stress shock is the liquidation switch.

**What would change it:** Order-level data (Hyperliquid node, L4 vendors) or more days of
recordings (weekends, more shocks): recalibrate. A shape difference that changes a
headline number: report both flows side by side (always). A headline that turns on price
speed at rates above 100k/s: let messages per flow-second follow the rate (it changes the
plan per rate, so arenas are no longer shared).

**Corrected on building** (in the text above; the decision itself is unchanged; PIPELINE.md
14.12 and 22):
- Spec: the draft cited PIPELINE.md 14.10, which is the sender; the flow's spec is 14.12.
- **Scale** (the owner to confirm): the draft said a flow-second holds as many messages as
  the offered rate. The generator holds a fixed `messages_per_flow_second`, 100,000, as the
  M3 flow does, so plans stay independent of the rate (14.1) and runs at different rates
  share one signed arena (14.8). Flow time is real time only at an offered 100k/s.
- Activity across markets: the table holds each market's own recorded weight, not draws
  of a lognormal; sigma 0.49 and 1.35 describe those weights.
- Book shape: gaps "from the tick pmf for levels 1–10" are the shares of gaps of 1 to 10
  real ticks, per level bucket; the clip jitter is ±3% (the draft: ±2–3%); dust is 1.3% to
  13.7% of levels by class (the draft: ~9%), mostly at levels 11–20 (the draft: 10–20).
- **Background sizes:** the recorded levels' own quantiles, not the lognormal the draft
  named: the sizes are bimodal (small orders and large ones), and a lognormal with their
  mean and sd of ln(USD) puts its largest values 3 to 10 times too high (tradfi equities'
  largest of 64: $5.9M from the lognormal, $0.86M recorded). The mean and sd are kept.
- Price dynamics: the mixture's sds are in units of each market's own RMS move
  (`flow.json` standardises by it), not bps; the profile carries each market's RMS.
- **Bursts:** normalised over each phase, not by the long-run mean `exp(V / 2)`: over one
  run the slow component barely moves (half-life 326 s in the median hour), so its level
  alone could put a run's load 20% away from `R`.
- Shocks: the switch is `--shock real|stress`; shocks come every 10 s of flow time; the
  movers within 52 ms; the calibrated size is 4 to 8 RMS with each mover keeping the
  direction with 95% chance (the draft: ~5 sigma).
- The report's `SetMark` time is its core service time, a new gate histogram
  (`stage.set_mark_core_service`), queueing left out.
- Trade-offs, memory: quantified above.

**Corrected after review** (2026-09-30, the repo's review of the build, PIPELINE.md 22; in
the text above; the owner to confirm the readings among them):
- **Default makers: 60, not 12** (still 3 per market, now in 20 groups dealt by maker
  weight). An account verifies on gateway `account mod N`, and 12 makers put two on
  gateways 1 and 2 of 10 (17.4% and 15.8% of the messages, against 7% to 10% on the
  others), so the default flow's signed ceiling would have measured `12 mod 10`, not
  Polymarket. 60 is a multiple of the gateway counts 1 to 6, 10, 12, 15, 20 and 30: at 10
  gateways each lane now carries 9.5% to 10.3%. `--makers K` is capped at 20, the quotes a
  side, since makers beyond 21 never quoted.
- **Book shape:** sizes are drawn per level, as recorded (level 1 was 3 to 17 times too
  deep, so takers rarely walked past it; fills per IOC now 1.36, recorded 1.35 prints per
  taker order); gaps are fitted on the sides that show all 20 levels, and a gap is drawn
  among those that stay within reach (level 20 sat at the reach, 3 to 10 times too far, in
  3 of 5 classes; now 1.1 to 2.2 times the recorded median); re-prices and spread changes
  draw what they move with the gaps next to it given their sum (a Gibbs step), and after a
  move of the fair value the ladders follow it whole (80 maker messages, taken from the
  message clock), so the book keeps the profile's gaps and spread (the top 1-tick share
  had fallen from about 60% to 40–53%). The spread is still distributed as the profile's,
  but each market's relaxes slowly instead of being a fresh draw each time.
- **IOCs:** the high-leverage adds are 100 of the takers' 695 ppm, in markets by taker
  weight, not 100 ppm on top in markets picked uniformly.
- **Shocks:** the Pareto's bounds are a clamp, not a condition (Readings, below).
- **Bursts:** each phase's multipliers average 1 over its expected length, `n / R`, not
  over `ceil(n / R) + 1` whole seconds, after which the timed flow ended up to a second
  before the window closed for 44% of seeds at the headline's shape (median preset).
- **The digest** hashes the profile JSON's SHA-256, so a regenerated table changes it.
- **Through the pipeline**, a maker's place can overtake another maker's cancel (the
  sequencer orders only within an account, PIPELINE.md 9.1) and be rejected
  `PostOnlyWouldCross`: rare (about 0.01% of places at 100k/s with the stress shock, as the
  review measured), never in plan order, documented rather than prevented.

**Readings taken while building** (PIPELINE.md 14.12 and 22; the owner to confirm):
- Markets: the band is `400,000 / max_leverage` ppm (Polymarket's `1 / max_leverage` breaks
  band rule 1, D-015; these pass both rules), the range half to twice the start price, the
  fees the M3 flow's classes (5x and 3x markets take the 10x class's); makers at
  `min(5, max_leverage)`, 60 of them in 20 groups of 3, the markets dealt to the groups by
  maker weight.
- Makers: re-price or resize chosen by the running split of maker messages (a fixed chance
  drifted to 76% adds and removes, against 73.2% recorded); a re-price at rank 0 is a
  spread change that moves both best quotes to a new spread; a re-price and a spread change
  draw the gaps (and the spread) next to what they move together, given their sum; after a
  move of the fair value the ladders follow it whole; a quote's reach is half the band, and
  a gap is drawn among those that stay within it.
- Shocks: every 10 s (the recorded rate is one per 347 s, so this is a stress setting);
  the calibrated size's last stretch up to 8 RMS; 95% of calibrated movers keeping the
  shared direction; the Pareto's bounds read as a clamp, every draw below 14 moving 14
  markets and every draw beyond 88 all 88: a median of 14 (recorded 14) and a 90th
  percentile of 38 (recorded 31), 4.1% of shocks beyond the recorded maximum of 71 and 3.1%
  moving all 88, since the fitted Pareto's tail is a little heavier than the 227 recorded
  events (built first as conditioned on at least 14: a median of 22, a 90th percentile of
  69, 7.1% at 88); the stress cascade at 8 accounts per market, $1M deposits and $5,000
  IOCs, re-entering half-way between shocks.
- Others: one account per taker cluster; 2 high-leverage accounts per market with IOCs of
  $2,000, adding at 100 of the IOCs' 695 ppm in markets by taker weight; the fund at $1,000; no operator withdrawals in this flow; `--bursts` allowed with
  the M3 flow too (it changes only the schedule); the durable limit always from the M3
  flow's pre-verified 20k/s runs, so every flow is judged against the same disk; from the
  profile, tradfi macro's class spread is one real tick, the taker cluster sizes are the
  recorded ones (1 to 54) unsmoothed, and the takers' share is 695 ppm from the clean
  window (`flow.json`: 697).
- **Open, the owner's call:** the default flow almost never liquidates. At the maximum
  leverage a fresh position is liquidated by a move of about `0.5 / max_leverage` (1% at
  50x, 2.5% at 20x), and the recorded jumps are 0.5% to 1.23%: only a 50x market's jump of
  about 0.95% or more liquidates (2 of the 16 jump sizes; 4 of the 88 markets are 50x).
  And the recording has no such jump: the 50x markets had no persistent move over 50 bps
  (the 20x ones 0.011 per market-hour, the 10x 0.13, the 3x and 5x 0.31), while the flow
  applies the pooled median rate to every market, a simplification, so its rare
  liquidations are an artefact. Per-class rates (0 at 50x) would remove them. Liquidations
  in the default flow would need, for example, entries at the band's edge or aged
  positions.

**Status (2026-09-30):** Built, opt-in (`e2e --flow polymarket`, with `--makers`,
`--bursts` and `--shock`); spec `docs/PIPELINE.md` 14.12, build log in its section 22.
Evidence, local (WSL2, Ryzen 7 2700X, built and tested in the `dev` container only):
- **The calibration:** `tools/calibrate/calibrate.py extract --check` and `generate
  --check` both say "identical" (extract: 3 min 6 s on 6 processes). Where it overlaps the
  first calibration it matches exactly: all 88 start prices, every market's inner level
  changes (66,281,072), taker events (46,093) and prints (62,006); the checker's corrected
  spreads are reproduced exactly from its hold-out hours.
- **Tests:** `loadgen` 91, 38 of them new (13 on the profile's table, 22 on the flow, 3 on
  bursts); `bench` 22 new (23 in the feature build), four of them Polymarket smoke runs
  (signed; pre-verified with 3 makers and bursts; EIP-712; libsecp256k1 in the feature
  build), each with no hot-thread allocation, no fault in the window, the replay
  identical, the audit clean, a liquidation and a shortfall. The whole workspace: 666
  tests in the default build and 675 with `c-secp256k1`, all passing (one unchanged
  `pipeline` timing test failed once in the feature build and passed on the rerun,
  PIPELINE.md 22); clippy with `-D warnings` and `fmt` clean in both; `Cargo.lock`
  unchanged; the M3 flow's plan, digest and pinned tests unchanged. After the review
  (PIPELINE.md 22, "Review of the Polymarket-shaped flow"): `loadgen` 97 (one more on
  the profile, four on the flow, one on bursts), the workspace 672 and 681, all passing,
  clippy and `fmt` clean, `Cargo.lock` unchanged.
- **The generator** (default seed, applied to a real engine in plan order; after the
  review's fixes): no reject in setup; 0.13% of timed client commands rejected, all
  `UnknownOrder` (never a post-only cross in plan order; through the pipeline, see
  "Corrected after review"). Over 10 s of flow: maker messages 36.60 / 36.60 / 13.16 /
  13.21% (recorded 36.6 / 36.6 / 13.4 / 13.4), levels 52.3 / 27.3 / 20.4% (52 / 27.7 /
  20.3); each level's median depth within 25% of the profile's; the shares of 1-tick and
  over-10-tick gaps within 4 points of the profile's in the three large classes (up to 7
  fewer over 10 ticks after level 10, drawn within reach); spreads within 20% of the
  profile's 10th, 50th and 90th percentiles; depth per quote pooled at the median within 9%
  of the recorded books, at the 90th percentile within 17%; the no-move share within 1.5
  points in every leverage class; the 60 makers 1.5% to 1.9% of maker messages each;
  `--makers 3`: 30% to 37% each; the stress shock every 2 s over 10 s: 84 liquidations
  (199 when shocks moved 22 markets at the median), all from marks, up to 5 in one
  command. The engine holds 249 MB after setup A (231 MB for
  the M3 flow; measured before the review, the reservation unchanged).
- **Local release runs** (unpinned, a shared machine: every run invalid as
  generator-limited, so not benchmark numbers; after the review's fixes): pre-verified at
  50k/s over 20 s, 0.049% engine rejects, the top 10 markets 23.0% of messages (recorded
  23%), the takers' top 10 55.7% (53%), 1.36 fills per IOC (1.35 prints per taker order),
  no hot-thread fault; at 20k/s with 10 lanes, each lane 9.5% to 10.3% of the messages;
  the stress shock at 100k/s over 25 s, 34 liquidations, 0.070% rejects, the longest
  `SetMark` core service 166 µs, at most 57 events from one command, no hot-thread fault;
  `--bursts median` at 2k/s with seeds 2, 3, 5 and 8 (all ending early before the fix):
  every flow ended after the window closed, the sender's faults known (0). Before the
  review: signed at 15k/s with 3 makers and bursts, the busiest maker 35.8% of messages.
- Headline pending PERPSBOX (`docs/RUNBOOK-PERPSBOX.md`, section 7b: with `k256` in the
  first session, with libsecp256k1 in the second).
