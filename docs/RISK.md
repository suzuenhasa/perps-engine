# Risk layer specification (Milestone 2)

Status: **draft for owner review**, 2026-09-29, revised the same day after three reviews
(numerics, exploits, engineering; section 18 logs every finding and what changed).
Decisions: `docs/DECISIONS.md` D-013 to D-020. Open questions for the owner are in
section 17.

This is the exact specification of everything the engine does above the order book:
accounts, isolated slots, the insurance fund, the pre-trade check, fills and fees,
collateral release, margin states, withdrawals, marks, the price-band sweep, the
liquidation index, liquidation and the operator commands. It is written so that an
engineer can implement it without guessing and a property test can check it. Every
formula is integer arithmetic. Every rounding direction is stated.

Sources: INFO.md section 4 (the plan), D-002 (fund takeover), D-003 (liquidation index),
D-004 (units), D-005 (records), D-008 (book semantics), Polymarket's instrument list
(`data/rest/instruments/2026-09-29.json`) and the Polymarket Perps FAQ
(https://docs.polymarket.com/perps/faq, read 2026-09-29). Quotes from the FAQ used here:
- "MMR = 0.5 / max_leverage, independent of position size and of your leverage setting —
  for example, 2.5% on a 20x market."
- Tiers raise "the initial margin rate on your entire position — the cap is not applied
  bracket by bracket."
- "WorstCaseSize = max(|Position + OpenBuys|, |Position - OpenSells|)".
- "Withdrawals must leave the greater of existing collateral reservations and 10% of total
  open-position notional at Mark Price."
- Fees are "abs(Price * Quantity) * Rate" per fill.

All worked numbers in this file were checked with throwaway integer models of this spec
(Python, not in the repo). The first model checked invariants I1 to I10, and that the
`SetMark` walk liquidates exactly the slots the direct check finds, after each of 486,825
random commands. After the review, a second model of the revised spec replayed tests T1 to
T6 and the reviewers' attack sequences event by event. It also checked I1 to I9, I16, the
cost bound of 2.4, the key assertions of 9.1 and the band claims of 5.3 after each of
360,000 random commands (about 68,000 fills, 3,400 liquidations and 9,500 orders cancelled
by the band sweep) on 20x, 50x (with tiers and a maker rebate), 10x and 3x markets, with
and without mark moves. That is evidence for the spec's arithmetic, not for the Rust code,
which gets its own property test (section 14.3).

## Contents

1. Scope
2. Units, limits and rounding
3. State
4. Money formulas
5. The pre-trade check and the price band
6. Commands: validation order and effects
7. Book events, fills and fees
8. The post-command pass: release, liquidation check, re-key
9. The liquidation index
10. Liquidation and the insurance fund
11. Event order reference
12. Invariants
13. Unit tests
14. Performance, ablations and the Mode seam
15. Type and trait changes
16. Choices made in this spec
17. Questions for the owner
18. Review log

---

## 1. Scope

In scope for Milestone 2: isolated margin only, one slot per (account, market), the O(1)
worst-case check, margin states, withdrawals, `SetLeverage`, `SetMark` with its
liquidation walk and price-band sweep, the liquidation index, insurance-fund takeover and
shortfall tracking.

Not in v1 (INFO.md section 12): cross margin, the liquidation waterfall and ADL,
liquidation orders on the book, `reduce_only`, FOK, cancel-all (including an operator
cancel), a proactive `MarginCall` event (a second index keyed by margin-call price),
funding, computed mark prices, a liquidation fee, Polymarket's per-order limits (minimum
notional, maximum order count), and changing fees or the band on a live market. Section 16
lists each of these where it touches a rule here.

The engine is one deterministic state machine. A command is applied completely before the
next one starts. A **rejected command changes no state at all**: no account or slot is
created, no sequence number advances, no collateral moves, no counter moves.

## 2. Units, limits and rounding

### 2.1 Units (D-004)

| Quantity | Type | Unit |
|---|---|---|
| price, mark | `Price = i64` | ticks of the market |
| quantity, position | `Qty = i64` | lots of the market; positions are signed (long > 0) |
| money | `Micros = i64` | micro-dollars (1 USDC = 1,000,000) |
| fee rates, price band | `i32` / `u32` | parts per million (ppm) of notional |
| leverage | `u16` | whole multiples (1 = 1x) |

One tick times one lot is exactly one micro, so **notional = price × qty**, with no scale
and no rounding. Example (D-004): 1 unit of SP500 at 7,502.4 is 75,024 ticks × 100,000
lots = 7,502,400,000 micros = $7,502.40.

### 2.2 Integer helpers

Every division in this spec has a positive divisor `b` and uses one of these, in the
integer width that section 2.4 gives for its operands (`i64` or `i128`):

```
floor_div(a, b) = a.div_euclid(b)            // rounds toward minus infinity
ceil_div(a, b)  = -floor_div(-a, b)          // rounds toward plus infinity
```

Note that Rust's `/` rounds toward zero, which is neither of these for negative `a`. The
spec never uses plain `/` on a value that can be negative.

### 2.3 Rounding rules

One principle: **the exchange rounds in its own favour, and anything that makes money
withdrawable is rounded down.** Each rule, where it is used, and why:

| Value | Formula | Direction | Why |
|---|---|---|---|
| IM, MM | `ceil_div` | up | a requirement is never understated |
| withdrawal reserve | `ceil_div(notional, 10)` | up | a requirement |
| fee (signed ppm) | `ceil_div(notional × ppm, 1,000,000)` | toward +inf | a fee rounds up; a rebate (negative) rounds toward zero, i.e. down in size |
| band edges | upper `floor_div`, lower `ceil_div` | toward the mark | the allowed band is never wider than the exact band; computed once per `SetMark` and stored |
| removed cost basis on a reduce | `ceil_div(cost × closed, abs(pos))` | toward +inf | realized PnL (which can be released to free) rounds down |
| liquidation key | exact (section 9) | none | the key equals the integer check exactly |

Equity, unrealized PnL and notional are exact integers and need no rounding.

### 2.4 Limits that rule out overflow

Two engine constants:

```
PRICE_LIMIT    = 2^32          // max_price of any market must be below this
NOTIONAL_LIMIT = 2^53 micros   // about $9.0 billion: the largest worst-case notional of one slot
```

Each market gets `max_qty = floor(NOTIONAL_LIMIT / max_price)`, computed at
`SetMarketParams`. Example: `max_price` 150,000 ticks gives `max_qty` = 60,047,995,031
lots (600,479 SP500 units, $4.5 billion at 7,502.4).

A place or modify is rejected with `SizeLimit` if the slot's worst-case size after it,
`W'` (section 4.2), would exceed `max_qty`. `W'` is computed in `i128`, because a client
can send any `i64` quantity.

What this bounds, for every non-fund slot:
- **Position.** `abs(pos) <= W <= max_qty` (fills never raise `W`, section 4.2), and every
  price and mark is at most `max_price`, so `abs(pos) × price <= 2^53`.
- **Cost basis.** `abs(cost) <= abs(pos) × max_price <= 2^53`, for longs and shorts alike.
  Opening adds the fill's notional, which keeps the bound. A flip leaves exactly
  `pos' × price` (section 7.3). A reduce from `s` lots to `s'` keeps
  `cost' = cost − ceil(cost × (s − s') / s) = floor(cost × s' / s)`. For a long that is at
  most `cost × s' / s <= s' × max_price`. For a short,
  `abs(cost') = ceil(abs(cost) × s' / s) <= ceil(s' × max_price) = s' × max_price`.
- **Unrealized PnL.** `abs(upnl) = abs(pos × M − cost) <= 2^53 + 2^53 = 2^54`.
- **Locked collateral.** At the end of the command that last touched a slot,
  `locked <= IM(W) + max(0, −upnl)` at that mark. (After the pass, either something was
  released and `locked` is `IM(W)` or `IM(W) − upnl`, or nothing was releasable and the
  same bound follows from `min(locked, E) <= IM(W)`; section 8.2. A top-up leaves
  `locked = IM(W') − upnl`.) `IM(W) <= W × M <= 2^53` and `abs(upnl) <= 2^54` at any mark,
  so `locked < 2^53 + 2^54 < 2^55` at every command boundary. Inside a command it can
  briefly hold realized profit on top, still below `2^56`. From below, `E >= 0` gives
  `locked >= −upnl >= −2^54`.
- **Equity.** `E = locked + upnl`, below `2^56` in absolute value.

Which width each computation uses (the `SizeLimit` check comes before the margin rule, so
every notional that reaches a formula is at most `2^53`):

| Width | Values |
|---|---|
| `i64` | notional, `W` (after the `SizeLimit` check), IM, MM, equity, upnl, release, top-up, band edges (`M × (10^6 + b) < 2^32 × 1,450,000 < 2^53`, because band rule 1 caps `b` at 450,000) |
| `i128` | `W'` before the `SizeLimit` check; fees (`notional × ppm`, below `2^72`); the products inside `apply_change` (`cost × qty`, below `2^106`); the key's numerator (below `2^73`) and denominator (below `2^70`); the fund's values; the withdrawal sums; every `SetMarketParams` check |

Aggregates that grow with the number of accounts or commands (an account's free balance,
the fund balance, the fund's per-market position and cost basis, fees collected) use
`checked_add`/`checked_sub`. A `Deposit` that would overflow its balance is rejected
(`InvalidAmount`), because it is input. Any other overflow of an aggregate is a bug or an
input far beyond real use (trillions of dollars); the engine then **panics** with the name
of the value rather than wrapping. A panic is deterministic, so replay reproduces it. The
fund's unrealized PnL and its running total are kept in `i128`.

## 3. State

### 3.1 Account

```
Account {
    free: Micros,          // the free balance; >= 0
    next_seq: u64,         // lowest order sequence number that is still acceptable
}
```

- An account is created by its first accepted `Deposit` or `SetLeverage`.
- `FUND` never has an `Account` entry. Its balance is `fund_balance` (section 3.4).
- `next_seq` starts at 0. An accepted `PlaceOrder` with sequence `s` (the low 32 bits of
  its `OrderId`) sets `next_seq = s + 1`. It is a `u64` so that `s = u32::MAX` still fits.
  So "the sequence must be above the account's last accepted one" (INFO.md, "Commands") is
  `s >= next_seq`. Rejected orders do not advance it, so a client may resend a rejected
  order with the same id. (M3 note: this is safe only if the signed-message nonce is used
  up by every message, including rejected ones; otherwise a relayer could replay a
  rejected signed order later, when it would pass.)

### 3.2 Slot (one per account and market)

```
Slot {
    pos: Qty,              // signed position in lots
    cost: Micros,          // signed cost basis: sum of (signed qty × price) of what is held
    locked: Micros,        // collateral held for this position and its orders (signed, see below)
    open_buys: Qty,        // sum of remaining qty of the account's resting buys in this market
    open_sells: Qty,       // same for sells
    leverage: u16,         // chosen leverage; 1 until SetLeverage
    // bookkeeping, not economic state (left out of the snapshot, section 15.5):
    indexed_key: Option<(Side, Price)>,  // what the liquidation index holds for this slot
    key_dirty: bool,       // pos, cost or locked changed since the last re-key
    touched_in: u64,       // command counter, for deduplicating touched slots (section 8.1)
}
```

- **Created in one place only**, `slot_or_create(market, account)`, which PlaceOrder and
  SetLeverage both call once the command is accepted. It inserts the slot into the
  market's `slots` map (every number 0, leverage 1, `indexed_key` None, `key_dirty` false,
  `touched_in` 0) and appends the account to `market.accounts`. Slots are never deleted, so
  the chosen leverage survives a liquidation.
- `cost` sign: a long has `cost >= 0`, a short `cost <= 0`, a flat slot `cost = 0`.
  Example: long 10 lots bought at 100 ticks has `cost = 1,000`; short 10 sold at 100 has
  `cost = −1,000`.
- `locked` is signed. It holds deposits moved in, realized PnL and fees. It can go below
  zero only in a corner case where realized losses exceed it while the remaining position
  has an unrealized profit that keeps equity at or above MM. A flat slot with negative
  `locked` would have negative equity; commands can't produce one (section 10.1), and the
  pass would liquidate it.

### 3.3 Market

```
Market<B> {
    params: min_price, max_price, maker_fee_ppm, taker_fee_ppm, price_band_ppm, max_leverage,
    max_qty: Qty,                        // floor(NOTIONAL_LIMIT / max_price)
    tiers: [(lower_bound: Micros, max_leverage: u16); 8], tier_count: u8,  // live table; 0 = none yet
    staged: [(Micros, u16); 8], staged_rows: u8, staged_count: u8,        // SetRiskTier batch in progress
    mark: Option<Price>,                 // None until the first SetMark
    upper: Price, lower: Price,          // band edges at the current mark (section 5.3)
    book: B,                             // the market's order book (M1)
    slots: IdMap<AccountId, Slot>,       // this market's slots, seeded hash (D-011)
    accounts: Vec<AccountId>,            // accounts with a slot here, in creation order
    fund_pos: Qty, fund_cost: Micros,    // the insurance fund's netted position here
    fund_upnl: i128,                     // fund_pos × mark − fund_cost at the current mark
    fees_collected: Micros,              // net fees (fees minus rebates) from this market's fills
    nonzero_positions: u32,              // slots here (fund included) with pos != 0
    index: LiquidationIndex,             // longs and shorts (section 9.3); empty without the index
}
```

`market.accounts` is kept in every mode (so the snapshots of all modes match) and is read
only by the naive `SetMark` (section 14.2).

A market exists once `SetMarketParams` has been accepted for it. It **opens in three
steps**: `SetMarketParams`, then a tier table (`SetRiskTier`, section 6.9), then `SetMark`.
Orders need a mark (`NoMark`) and `SetMark` needs a tier table (`NoRiskTiers`), so no
order is ever checked against a missing or half-written table.

### 3.4 The insurance fund

- It is the account `FUND = AccountId::MAX` (4,294,967,295). No client may use that id.
- It is capitalised by an ordinary journaled `Deposit` to `FUND`, which adds to
  `fund_balance`. There is no other way in, and no way out in v1.
- It has no `Account` entry and no slots. Its state is `fund_balance` (engine-wide) and
  `fund_pos`, `fund_cost`, `fund_upnl` per market.
- **Exemptions:** no pre-trade check, no margin states, not in the liquidation index.
  Otherwise the positions it absorbs, which start at or near zero equity, would be
  liquidated back into itself.
- **It cannot trade in v1.** Orders whose `account_of(order_id) == FUND` are rejected
  (`ReservedAccount`), as are `Withdraw` and `SetLeverage` for `FUND`. (Question Q2.)
- Fund equity = `fund_balance + fund_upnl_total`, where
  `fund_upnl_total = Σ over markets of fund_upnl`, kept as a running `i128` total.
- Uncovered bad debt = `max(0, −fund equity)`.

### 3.5 Engine-wide

```
Engine<B: OrderBook, M: Mode> {
    markets: Vec<Option<Market<B>>>,     // indexed by MarketId; grown by SetMarketParams
    accounts: IdMap<AccountId, Account>, // seeded hash (D-011); FUND has no entry
    fund_balance: Micros, fund_upnl_total: i128,
    last_reported_uncovered: Micros,     // the value in the last InsuranceShortfall (0 at start)
    net_deposits: i128,                  // Σ Deposit − Σ Withdraw, for the conservation check
    command_counter: u64,
    scratch: Vec<Event>,                 // the book's events for the current call (section 7.1)
    touched: Vec<AccountId>,             // slots of the command's market it touched (section 8.1)
}
```

- A market lookup is an index into `markets` (O(1)). The engine never iterates `accounts`
  or a market's `slots` map where order matters: it only looks entries up. `Withdraw`
  loops over `markets` in id order; the naive `SetMark` reads `market.accounts`.
- Every command acts on at most one market, so `touched` holds account ids.
- **`command_counter`** starts at 0 and increases by 1 as the first step of every accepted
  command. Rejected commands leave it unchanged. A new slot starts with `touched_in = 0`,
  which no accepted command uses (the counter is at least 1 by then).
- `scratch` and `touched` are reserved at start-up (`EngineOptions`, section 15.5) and
  cleared, never shrunk, so a command that fits in them does not allocate. `touched`
  never holds more entries than `max(1, scratch.len())`: apart from the command's own
  slot, each touched slot has at least one event in `scratch` (a fill or a swept cancel).
  So it gets the same capacity as `scratch`.
- The engine walks `scratch` and `touched` by index (`for i in 0..self.scratch.len()
  { let e = self.scratch[i]; ... }`; `Event` is `Copy`), so it can update itself inside
  the loop without borrowing problems or allocation.

## 4. Money formulas

Notation for one slot in one market: `M` = current mark, `Lmax` = the market's
`max_leverage`.

### 4.1 Notional, unrealized PnL, equity

```
notional(qty, price) = qty × price                        // exact
upnl(M)   = pos × M − cost                                // signed, exact
equity(M) = locked + upnl(M) = locked + pos × M − cost
```

Example: long 1,000 lots with cost 100,000,000 (bought at 100,000) and locked 5,000,000.
At mark 98,000: `upnl = 98,000,000 − 100,000,000 = −2,000,000`, equity = 3,000,000.

### 4.2 Worst-case size

```
W = max(abs(pos + open_buys), abs(pos − open_sells))
```

`W >= abs(pos)`. Fills never raise `W`: a buy fill of `q` moves `pos` up by `q` and
`open_buys` down by `q`, so `pos + open_buys` is unchanged and `pos − open_sells` moves
toward it. So `W` can only grow through an accepted place or modify, which is where
`SizeLimit` and the margin check apply.

### 4.3 Effective leverage and tiers

For a size `X` (in lots) at mark `M`, with the slot's chosen `leverage`:

```
N         = X × M                                   // notional at mark
tier_max  = max_leverage of the last tier with lower_bound <= N
lev_eff   = min(leverage, tier_max)
```

The tier is picked by the notional **of the size being margined, at the mark**: `W' × M`
in the pre-trade check, `W × M` for a release, `abs(pos) × M` for the margin state. The
rate `1 / lev_eff` applies to the whole notional, not bracket by bracket (Polymarket FAQ).
A notional exactly at a tier's `lower_bound` is in that tier.

### 4.4 Initial and maintenance margin

```
IM(X) = ceil_div(X × M, lev_eff(X × M))
MM    = ceil_div(abs(pos) × M, 2 × Lmax)          // MMR = 0.5 / Lmax, flat per market
```

`IM(0) = 0`, `MM = 0` for a flat slot. Because tiers only lower leverage as notional grows,
`IM(X)` never decreases as `X` grows. Because `lev_eff <= Lmax`, `IM(abs(pos)) >= MM` and
`IM(X) >= X × M / Lmax`. MM does not depend on the tiers or the chosen leverage.

Example (SP500's real tiers: 50x from $0, 25x from $500,000; chosen leverage 50; mark
75,024):
- `X` = 6,000,000 lots: `N` = 450,144,000,000 ($450,144), tier 50x, IM = 9,002,880,000.
- `X` = 8,000,000 lots: `N` = 600,192,000,000 ($600,192), tier 25x, `lev_eff = 25`,
  IM = 24,007,680,000 (4% of the whole notional).

Example MM: on a 20x market, long 100,000 lots at mark 73,100:
`MM = ceil_div(7,310,000,000, 40) = 182,750,000`.

### 4.5 Margin states

Per slot, at the current mark, based on the position's size:

| State | Condition |
|---|---|
| Healthy | `equity >= IM(abs(pos))` |
| Margin call | `MM <= equity < IM(abs(pos))` |
| Liquidation | `equity < MM` |

States are not stored and emit no event (a proactive `MarginCall` event is "Later"). They
are evaluated:
- in the pre-trade check of every place or replacing modify (section 5.2), to choose
  between `MarginCall` and `InsufficientMargin` when a top-up is impossible;
- in the post-command pass for every touched slot (`equity < MM` means liquidate,
  section 8);
- at `SetMark`, through the liquidation index (section 9.4).

Invariant I5 (section 12) makes "Liquidation" impossible at a command boundary: any slot
that reaches it is liquidated before the command ends.

## 5. The pre-trade check and the price band

### 5.1 Which orders are checked

For a place or modify on side `side`, let `open_buys'` and `open_sells'` be the slot's
open totals **as if the new order (or the modified order's new remaining quantity) were
resting in full**:
- place buy of `q`: `open_buys' = open_buys + q`; place sell: `open_sells' = open_sells + q`.
- modify that replaces an order with remaining `r` by new remaining `r'`: the order's side
  total changes by `r' − r`.

`W' = max(abs(pos + open_buys'), abs(pos − open_sells'))`.

**Pure decreases skip every risk check.** A modify with the same price and a new remaining
at or below the old one (shrink in place), a modify at or below the filled quantity
(removal, `SizeBelowFilled`), and a cancel are always accepted when the order exists. They
add no fill volume.

**Strictly reducing** (INFO.md): the order adds only to the side that shrinks the
position, and that side's total stays within the position:

```
strictly_reducing = (pos > 0 and side == Sell and open_sells' <= pos)
                 or (pos < 0 and side == Buy  and open_buys'  <= −pos)
```

A flat slot has no strictly reducing orders. A strictly reducing order never raises `W`
(for a long, `pos − open_sells'` stays in `[0, pos]`). Strictly reducing orders skip the
margin rule (no top-up, no `MarginCall`, no `InsufficientMargin`), so a margin-called
trader with no free balance can still reduce or close. They still pass the band and the
other checks.

### 5.2 The margin rule

For every other place or replacing modify (including a flip that leaves `W` unchanged):

```
E    = equity(M)
need = max(0, IM(W') − E)
if need <= account.free:        accept; move `need` from free to locked (the top-up)
else if E < IM(abs(pos)):       reject MarginCall        // the slot is in margin call
else:                           reject InsufficientMargin
```

- The requirement is valued at the current mark. Unrealized profit counts toward `E`.
- The top-up is "just enough": after it, `E = IM(W')` exactly (if `need > 0`).
- The order's own future fee is not reserved (section 16, C5).
- An account with no deposit (or unknown to the engine) has `free = 0`: its first order
  is rejected `InsufficientMargin` (its slot is flat, so not in margin call). No account or
  slot is created.

**Margin call and top-ups (recommended reading; owner to confirm, Q1).** A slot in margin
call whose account has enough free balance is topped up to `IM(W')` and the order is
accepted; after the top-up the slot is healthy. Only when the free balance cannot cure it
is a non-reducing order rejected with `MarginCall`. The literal alternative ("in margin
call, reject every non-reducing order") is one extra line before `need`:
`if E < IM(abs(pos)): reject MarginCall`. Section 17 explains why this matters.

Example (top-up): account A has free 20,000,000 and chosen leverage 20. It buys 1,000
lots at mark 100,000. `W' = 1,000`, `IM(W') = ceil_div(100,000,000, 20) = 5,000,000`,
`E = 0`, so `need = 5,000,000`. Free becomes 15,000,000, locked 5,000,000.

Example (flip still checked): A is long 1,000 with `E = 3,000,000` and places a sell of
2,000. `open_sells' = 2,000 > pos`, so it is not strictly reducing. `W' = max(1,000,
1,000) = 1,000`, `IM(W') = 5,000,000`, `need = 2,000,000`. With free 0 and
`E < IM(1,000) = 5,000,000`, the result is `MarginCall` (test T5).

### 5.3 Price band

A limit price more than the band through the mark is rejected (`PriceBand`). With
`b = price_band_ppm`, the edges are computed once per `SetMark` and stored in the market:

```
upper = floor_div(M × (1,000,000 + b), 1,000,000)     // buys above this are rejected
lower = ceil_div (M × (1,000,000 − b), 1,000,000)     // sells below this are rejected
```

Buys below the mark and sells above it are never band-rejected. The band applies to every
place and every replacing modify (at its new price), including strictly reducing ones.
When the mark moves, resting orders that the new band would reject are cancelled (the
sweep, section 5.4), so every resting order is always inside the band of the current mark
(invariant I16).

**Allowed band** (D-015). `SetMarketParams` is accepted only if both rules hold, with
`f = max(taker_fee_ppm, maker_fee_ppm)` and every value widened to `i128`:

```
Rule 1 (the owner's rule, with a 10% margin):
    20 × Lmax × (b + f) <= 9,000,000
Rule 2 (exact: includes the fee on the band and fee rounding):
    S = 10^12 − 2 × Lmax × (10^6 × (b + f) + f × b)
    S > 0  and  min_price × S >= 2 × Lmax × 10^12
```

Rule 1 is `2 × (b + f) <= 0.9 / Lmax`: INFO.md's `2 × (b + f) < 1 / Lmax` with a 10%
safety margin. Rule 2 is what the argument below actually needs. In plain words: the 10%
margin is worth `0.1 × M / Lmax` micros per lot, rounding a fee up can cost up to
2 micros per lot (two fills of one lot each), so the mark must be at least about
`20 × Lmax` ticks. Rule 2 applies it to the lowest possible mark, `min_price`, and also
catches the one case where rule 1's margin is too small for the `f × b` term (`Lmax = 1`
with large fees; review L1).

| Lmax | Largest band at taker fee 400 ppm (rule 1) | Smallest `min_price` for that band (rule 2) | Polymarket `price_bounds` (= 1 / Lmax) |
|---|---|---|---|
| 50 | 8,600 ppm (0.86%) | 1,004 | 20,000 ppm (2%) |
| 20 | 22,100 ppm (2.21%) | 402 | 50,000 ppm (5%) |
| 10 | 44,600 ppm | 201 | 100,000 ppm |
| 5 | 89,600 ppm | 101 | 200,000 ppm |
| 3 | 149,600 ppm | 61 | 333,333 ppm |

Real markets are far above these floors: the lowest mark recorded on 2026-09-29 across
Polymarket's 88 markets, in ticks, was 4,168 (KPEPE-USD, 10x, which needs 201). The unit
tests use `min_price` 1,000.

Example: SP500 at mark 75,024 (7,502.4) with `Lmax = 50` and band 8,600 ppm: upper =
`floor(75,669.2064)` = 75,669 (7,566.9), lower = `ceil(74,378.7936)` = 74,379 (7,437.9).
A buy at 75,669 is accepted; at 75,670 it is rejected.

**What the band guarantees.** Write `g = b + f + f × b` as a fraction (in ppm,
`g × 10^12 = 10^6 × (b + f) + f × b`). One fill of `q` lots inside the band of the mark
`M`, with its fee, lowers the slot's equity at `M` by less than `q × M × g + 1` micros:
the price is at most `M × b` through the mark, the fee is at most `f` of a price of at
most `M × (1 + b)`, and rounding the fee up adds less than 1 micro. Rule 2 says
`M / Lmax >= 2 × (M × g + 1)` for every mark `M >= min_price`. Three consequences, at a
fixed mark:

- **Claim 1 (orders just checked).** If `E >= W × M / Lmax`, no sequence of fills of the
  slot's orders takes `E` below zero. This holds right after an accepted margin check
  (`E >= IM(W') >= W' × M / Lmax`) and after any release (`E >= IM(W)`). Proof: the
  orders fill at most `open_buys + open_sells <= 2W` lots in at most `2W` fills (for a
  long, `open_buys <= W − pos` and `open_sells <= W + pos`), so they lose less than
  `2W × (M × g + 1) <= W × M / Lmax <= E`.
- **Claim 2 (reducing fills).** At every command boundary `E >= MM >= abs(pos) × M /
  (2 × Lmax)` (invariant I5). Fills that only reduce the position total at most
  `abs(pos)` lots in at most `abs(pos)` fills, so they lose less than
  `abs(pos) × (M × g + 1) <= abs(pos) × M / (2 × Lmax) <= E`. So a closing trade never
  creates bad debt, and no slot can end a command flat with negative collateral.
- **Claim 3 (the residual risk, accepted in v1).** Orders that open or grow a position
  are covered by claim 1 only while `E >= W × M / Lmax`. A mark move against the position
  lowers `E` without any check (the slot stays above MM, so nothing happens). If those
  orders then fill, the slot can end below zero, and the fund absorbs the difference. The
  loss is less than `q × M × g + (number of fills) − E` for `q` filled lots: at most about
  `g` of the filled notional (0.9% at 50x, 4.5% at 10x). Example (review F3; SP500's live
  parameters, `Lmax` 50, band 8,600, fees 400/125): A is long 100,000 at 75,024 and rests
  a bid of 500,000 at 67,261 (top-up to `IM(600,000)` = 900,288,000). The mark falls 11.1%
  in small steps to 66,688, where `E = MM = 66,688,000` and the bid is still inside the
  band (`upper` = 67,261). A colluder sells into the bid; A's equity becomes −224,015,813
  and the fund loses exactly that, 0.67% of the order's notional. Closing this needs a
  second index keyed by the mark at which a slot's growing orders stop being covered,
  which is INFO.md's "Later" margin-call index. A unit test pins the number (section 13).

Fills can still land a slot between zero and MM (test T4); the post-command pass
liquidates it and the fund absorbs non-negative equity. So bad debt needs a mark move:
a gap past a position's bankruptcy price before the walk reaches it (D-002), or claim 3.

The claims need every resting order to be inside the band of the **current** mark, not
just of the mark at which it was placed. Without that, a stale order fills far through the
mark and its owner loses the whole drift (reviews H1 and F1). The sweep (5.4) provides it.

Polymarket's `price_bounds` of `1 / Lmax` would let a band-edge flip lose twice the
initial margin, so fills alone could create bad debt. That is why the derived band is
tighter; matching Polymarket is "Later" (D-015).

### 5.4 The SetMark sweep

After its liquidation walk (section 9.4), `SetMark` cancels every resting order that the
new band would reject:
- bids priced above the new `upper`, highest price first;
- then asks priced below the new `lower`, lowest price first;
- oldest first within one price level.

Each emits `Cancelled { reason: PriceBand }`. The engine subtracts its remaining quantity
from the owner's side total and adds the owner's slot to `touched`. Then the post-command
pass (section 8) runs over those slots: it releases collateral (their `W` can only have
fallen) and re-keys them. It never liquidates, because cancels don't change equity and the
walk has just removed every slot below MM. If the mark rose, only asks can be out of band;
if it fell, only bids.

Why: an order placed inside the band of an old mark can be far through the current one.
Two reviewers built sequences of ordinary client commands that make the fund pay without
any gap:
- **F1.** A (20x, 5,000,000 deposited, fees 0) rests a bid of 1,000 at 100,000 and an ask
  of 1,000 at 100,001 at mark 100,000, one IM for both. The mark drifts to 80,000 in 1%
  steps; A is flat, so nothing looks at it. B then sells 1,000 at 100,000 (above the mark,
  so the band allows it) into A's stale bid. A is liquidated at equity −15,000,000 and the
  fund loses 15,000,000, while B holds a profit of 20,000,000.
- **H1.** A stale strictly reducing sell, placed at the lower band edge of an old, lower
  mark, stays resting after the mark returns. A then cancels other orders, the release
  returns the collateral that was backing them, and a colluder buys the stale sell; the
  fund loses 7,000,002 at an unchanged mark.

With the sweep, both sequences end with the fund unchanged: in F1 the bid is cancelled at
the first mark where `upper` drops below 100,000 (test T6), and in H1 the sell is
cancelled when the mark returns to 100,000.

Cost: O(1 + orders cancelled) in the book, which walks from its best price inward and
stops at the first level inside the band, plus one release and O(log n) re-key per owner.
A `SetMark` that cancels nothing costs two comparisons (best bid against `upper`, best ask
against `lower`).

Clients see a new behaviour: an order can disappear on a mark move, with reason
`PriceBand`. The M3 synthetic makers must treat it like any cancel they didn't ask for.
(Question Q3.)

## 6. Commands: validation order and effects

The checks run in the order listed. The first failing check produces a single `Reject`
and nothing else. **Every check the book would make is made by the engine first**,
including the existence of the order for a cancel or modify (`book.order(id)`), so the
book never rejects on the engine's path and no collateral moves for an order the book then
refuses. The engine still checks the book's first event with `assert!` (in release builds
too): `Ack` for a place, `Modified` for a replace or shrink, `Cancelled { SizeBelowFilled }`
for a removal, `Cancelled { UserRequested }` for a cancel. A mismatch is a bug and panics,
deterministically, rather than leaving a half-applied command.

`Reject { order_id, account, reason }`: for commands without an order, `order_id` is 0.
`account` is the order's owner or the command's account; for market-level commands
(`SetMark`, `SetMarketParams`, `SetRiskTier`) it is `AccountId::MAX`, an id no client can
hold, which consumers read as "operator".

### 6.1 PlaceOrder

| # | Check | Reject reason |
|---|---|---|
| 1 | the market exists | `UnknownMarket` |
| 2 | `account_of(order_id) != FUND` | `ReservedAccount` |
| 3 | `qty >= 1` | `InvalidQty` (as the book) |
| 4 | `min_price <= price <= max_price` | `InvalidPrice` (as the book) |
| 5 | `seq(order_id) >= account.next_seq` (0 for an unknown account) | `Duplicate` |
| 6 | not (`post_only` and the price reaches the best opposite price) | `PostOnlyWouldCross` (same test as the book, using `best_bid`/`best_ask`) |
| 7 | the market has a mark | `NoMark` |
| 8 | price inside the band (5.3) | `PriceBand` |
| 9 | `W' <= max_qty` | `SizeLimit` |
| 10 | strictly reducing, or the margin rule passes (5.2) | `MarginCall` / `InsufficientMargin` |

The book's own duplicate check can never fire: every resting order of the account has a
sequence below `next_seq`.

On acceptance, in this order:
1. `command_counter += 1`.
2. `slot_or_create(market, account)` (3.2).
3. `next_seq = seq + 1`.
4. Add `qty` to the order's side total (`open_buys` or `open_sells`). An IOC counts in full
   too; its remainder is taken off when the book cancels it.
5. Apply the top-up, if `need > 0`.
6. Clear `scratch`, call `book.place(order, &mut scratch)`, assert that `scratch[0]` is
   `Ack`, and post-process the scratch (section 7.1).
7. Run the post-command pass (section 8) and the shortfall report (section 10.4).

### 6.2 CancelOrder

| # | Check | Reject reason |
|---|---|---|
| 1 | the market exists | `UnknownMarket` |
| 2 | the order rests in this book (`book.order(id)` is `Some`) | `UnknownOrder` |

On acceptance: `command_counter += 1`; the book emits `Cancelled { UserRequested }`; the
engine subtracts its `remaining` from the side total, then runs the post-command pass on
the owner's slot (which releases collateral if the requirement shrank). Cancels are
accepted in any margin state. Ownership is the gateway's check (D-005), not the engine's.

### 6.3 ModifyOrder

| # | Check | Reject reason |
|---|---|---|
| 1 | the market exists | `UnknownMarket` |
| 2 | the order rests in this book (`book.order(id)`) | `UnknownOrder` |
| 3 | `new_size >= 1` | `InvalidQty` |
| 4 | `min_price <= new_price <= max_price` | `InvalidPrice` |

Then, with `r` = the order's remaining, `filled` its filled quantity, and
`r' = new_size − filled` (D-008, total-size semantics), the modify is one of three kinds:

- **Removal** (`r' <= 0`): always accepted. The book emits
  `Cancelled { SizeBelowFilled, remaining: r }`; the engine subtracts `r` from the side
  total, so `W` can only fall. Post-command pass (release).
- **Shrink in place** (`new_price == price` and `r' <= r`): always accepted. The book emits
  `Modified`; the engine subtracts `r − r'` from the side total. Post-command pass
  (release). A modify to the same size is in this group and changes nothing. The price is
  unchanged, so the order stays inside the band (I16).
- **Replace** (anything else): further checks, in order:

| # | Check | Reject reason |
|---|---|---|
| 5 | not (`post_only` and `new_price` reaches the best opposite price) | `PostOnlyWouldCross` |
| 6 | the market has a mark | `NoMark` |
| 7 | `new_price` inside the band | `PriceBand` |
| 8 | `W' <= max_qty` (side total changed by `r' − r`) | `SizeLimit` |
| 9 | strictly reducing (with the new side total), or the margin rule passes | `MarginCall` / `InsufficientMargin` |

  On acceptance: `command_counter += 1`; change the side total by `r' − r`, apply the
  top-up, call `book.modify`, post-process (the book emits `Modified`, then fills and
  self-trade cancels like a new order), then the post-command pass.

For removal and shrink in place, acceptance also starts with `command_counter += 1`. A
rejected modify leaves the original order untouched (the book's rule too).

### 6.4 Deposit

| # | Check | Reject reason |
|---|---|---|
| 1 | `amount >= 1`, and the target balance + amount fits in `i64` | `InvalidAmount` |

Effect: the account's `free` (or `fund_balance` for `FUND`) grows by `amount`;
`net_deposits += amount`. Creates the account if needed (never for `FUND`, which has no
`Account` entry). Event: `BalanceChanged` with the new balance. For `FUND`, then the
shortfall report (the uncovered amount may shrink).

### 6.5 Withdraw

| # | Check | Reject reason |
|---|---|---|
| 1 | `account != FUND` | `ReservedAccount` |
| 2 | `amount >= 1` | `InvalidAmount` |
| 3 | `free − amount >= 0` (free is 0 for an unknown account) | `InsufficientBalance` |
| 4 | `(free − amount) + Σ locked >= ceil_div(Σ abs(pos) × mark, 10)` | `WithdrawalReserve` |

The sums loop over `markets` in id order and look up the account's slot in each (O(number
of markets), about 88), in `i128`. Every market with a position has a mark. Unrealized PnL
is not counted either way; `locked` already holds each slot's IM, which is Polymarket's
"existing collateral reservations". Withdrawals are off the hot path, so this loop is fine.
Effect: `free −= amount`, `net_deposits −= amount`, event `BalanceChanged`. Withdraw never
touches a slot.

Example (test T2): a $100 position at 20x holds 5,000,000 locked; the reserve is
`ceil_div(100,000,000, 10) = 10,000,000`. So the account must keep 5,000,000 of free
balance: "$5 initial margin but a $10 withdrawal reserve".

### 6.6 SetLeverage { account, market, leverage }

| # | Check | Reject reason |
|---|---|---|
| 1 | the market exists | `UnknownMarket` |
| 2 | `account != FUND` | `ReservedAccount` |
| 3 | `1 <= leverage <= max_leverage` | `InvalidLeverage` |
| 4 | if the slot's `W > 0`: `need <= free`, where `need = max(0, IM(W) − E)` at the new leverage | `InsufficientMargin` |

Effect: `command_counter += 1`; create the account if needed and call
`slot_or_create(market, account)`; store the leverage; emit `LeverageSet`. If `W > 0`:
apply the top-up (if `need > 0`), then run the post-command pass on the slot, which
releases collateral if the new requirement is lower. `SetLeverage` is never rejected for
margin call: it can only add collateral or return what is above the requirement. With the
same leverage it therefore works as an operator "add margin to this position". Changing
leverage never changes the slot's liquidation key directly (the key depends only on
`pos`, `cost`, `locked` and `Lmax`), only through the collateral it moves. (A slot with
`W > 0` has orders or a position, so its market has a mark and a tier table.)

### 6.7 SetMark { market, price }

| # | Check | Reject reason |
|---|---|---|
| 1 | the market exists | `UnknownMarket` |
| 2 | the market has a tier table (`tier_count >= 1`) | `NoRiskTiers` |
| 3 | `min_price <= price <= max_price` | `InvalidPrice` |

Effect, in order:
1. `command_counter += 1`; `mark = price`; compute and store `upper` and `lower` (5.3);
   emit `MarkPrice`.
2. Fund PnL in O(1): `new = fund_pos × price − fund_cost` (i128);
   `fund_upnl_total += new − fund_upnl`; `fund_upnl = new`.
3. Liquidate what the new mark crosses (section 9.4): longs first, highest key first; then
   shorts, lowest key first.
4. The sweep (section 5.4): clear `scratch`, call
   `book.cancel_beyond(Buy, upper, PriceBand, &mut scratch)` and
   `book.cancel_beyond(Sell, lower, PriceBand, &mut scratch)`, then walk `scratch`.
5. The post-command pass over the owners of swept orders (release and re-key only).
6. Shortfall report (section 10.4).

`SetMark` touches only the slots it liquidates and the owners of the orders it sweeps. It
never visits the other slots: releasing on every mark move would mean visiting every slot,
which INFO.md rules out.

### 6.8 SetMarketParams

| # | Check | Reject reason |
|---|---|---|
| 1 | `1 <= min_price <= max_price < PRICE_LIMIT`; `max_price − min_price + 1 <= 2^24` (the book's `MAX_LEVELS`); `max_leverage >= 1`; `taker_fee_ppm >= 0`; `maker_fee_ppm + taker_fee_ppm >= 0`; band rules 1 and 2 (5.3) | `InvalidParams` |
| 2 | if the market exists, it is **empty** | `MarketNotEmpty` |

Every check is computed in `i128` after widening each field: the fields are `u32`, `i32`
and `u16`, and `20 × Lmax × (b + f)` alone can reach about `2^53`.

**Empty** means: no resting orders (`best_bid` and `best_ask` are both `None`) and no
positions (`nonzero_positions == 0`, the fund included). Both are O(1). A flat slot with no
orders always has `locked = 0` (invariant I7), so no collateral is stranded.

Effect: `command_counter += 1`; store the parameters; compute `max_qty`; build a new empty
book for the price range (`OrderBook::with_config`); clear the tier table and any staged
rows (`tier_count = 0`, `staged_rows = 0`); clear the mark. The market then needs a new
tier table and a new `SetMark` before it takes orders (3.3). `fees_collected` and the slots
(flat, with their chosen leverage) are kept. A stored leverage above the new maximum is
simply capped by `lev_eff`. Event: `MarketParamsSet` (echoes the command).

`maker_fee_ppm + taker_fee_ppm >= 0` makes every fill's net fee non-negative:
`ceil(a) + ceil(c) >= a + c = notional × (maker_fee_ppm + taker_fee_ppm) / 10^6 >= 0`, so
`fees_collected` never falls.

**Known limit.** Any client can keep a market non-empty with one resting 1-lot order or a
1-lot position, and a fund position keeps it non-empty for good (Q2). So once a market
trades, its price range, fees, band and `max_leverage` are in practice fixed. v1 accepts
this: markets are configured before they open, and the tier table, the main risk lever,
can change on a live market (6.9). Changing fees or the band on a live market, and an
operator cancel, are "Later" (C15).

### 6.9 SetRiskTier { market, index, count, lower_bound, max_leverage }

A tier table is sent as `count` rows, indexes 0 to `count − 1`, and takes effect all at
once when its last row is accepted.

| # | Check | Reject reason |
|---|---|---|
| 1 | the market exists | `UnknownMarket` |
| 2 | `1 <= count <= 8` and `index < count`; `1 <= max_leverage <=` the market's `max_leverage`; if `index == 0`: `lower_bound == 0`; if `index > 0`: the row continues the batch being staged (`index == staged_rows` and `count == staged_count`), `lower_bound > staged[index − 1].lower_bound` and `max_leverage <= staged[index − 1].max_leverage` | `InvalidParams` |

Effect: `command_counter += 1`; `staged[index] = (lower_bound, max_leverage)`. A row with
index 0 starts a new batch (`staged_count = count`, discarding rows staged before).
`staged_rows = index + 1`. When `staged_rows == count`, the batch is **committed in one
step**: `tiers` = the staged rows, `tier_count = count`, `staged_rows = 0`. Event:
`RiskTierSet` (echoes the command; the row with `index == count − 1` is the commit).

- Staged rows are invisible: until the commit, every check uses the old table. Each row is
  checked against the row below it, so a committed table always starts at 0, with bounds
  rising and leverage never rising.
- A rejected row changes nothing, staging included. The operator resends that row, or
  restarts the batch with index 0.
- **A commit is allowed on a live market.** Tiers change only IM. MM and the liquidation
  keys depend only on the market's `max_leverage` (4.4, 9.1), so no slot needs a re-key and
  none is visited. Each slot meets the new table at its next check or release: a slot whose
  IM went up may now be in margin call; one whose IM went down gets the excess back at its
  next touch. The band argument (5.3) uses `W × M / Lmax`, which tiers don't change.

Example: SP500's 8 tiers are 8 commands with `count` 8 and indexes 0 to 7; `lower_bound`
is in micros ($500,000 = 500,000,000,000). The new table applies once index 7 is accepted.
A single-tier market sends one row: `{ index 0, count 1, lower_bound 0, max_leverage }`.

## 7. Book events, fills and fees

### 7.1 The scratch sink

The engine never passes its output sink to the book. For each book call it:
1. clears `scratch` and calls the book with `&mut scratch`;
2. walks `scratch` in order, and for each book event applies it to the ledger and emits it
   to the real sink, followed by the events it causes:

| Book event | Ledger effect | Emitted |
|---|---|---|
| `Ack` / `Modified` (first event of an accepted place / replace) | none | the event, then the top-up events if there was a top-up |
| `Fill` | fees, both slots, open totals (7.2 to 7.4); maker's slot added to `touched` | `Fill` with fees filled in, `PositionChanged(maker)`, `PositionChanged(taker)` |
| `Cancelled { SelfTrade }` | the taker's own slot: subtract `remaining` from the cancelled order's side | the event |
| `Cancelled { IocRemainder }` | subtract `remaining` from the taker's side | the event |
| `Cancelled { UserRequested / SizeBelowFilled }` | subtract `remaining` from that side | the event |
| `Cancelled { PriceBand }` (the `SetMark` sweep) | subtract `remaining` from that side; owner's slot added to `touched` | the event |
| `Cancelled { Liquidation }` | none (the slot's totals are zeroed by the liquidation) | the event |
| `Reject` | never happens: the engine makes every check first (section 6) | the `assert!` on the first event panics |

Every event the book emits carries what the ledger needs (the side of a cancelled order,
the taker's side of a fill, section 15), so this step needs no other context. The book
runs to completion before any ledger update, so matching never sees a half-updated slot.
"Positions sum to zero" and the conservation identity hold after each event block (12),
because each fill updates both slots and the fees in one step.

`scratch` is reserved at start-up (`EngineOptions::scratch_capacity`, for example 4,096).
One book call emits at most `n + 2` events for a book of `n` resting orders: a place emits
`Ack`, at most one fill or self-trade cancel per resting order it reaches, and an IOC
remainder; a modify, `cancel_account` or `cancel_beyond` emit fewer. So a 4,096
reservation grows only for a call that reaches more than 4,094 resting orders. It grows
once and is never shrunk.

### 7.2 Fees

For a fill of `q` lots at price `P`:

```
maker_fee = ceil_div(P × q × maker_fee_ppm, 1,000,000)
taker_fee = ceil_div(P × q × taker_fee_ppm, 1,000,000)
```

A negative fee is a rebate. Each fee is taken from (or a rebate added to) that slot's
`locked`, never the free balance, so a trader with no free balance can still close.
`fees_collected += maker_fee + taker_fee`.

Examples (Polymarket's $0 tier: taker 400 ppm, maker 125 ppm):
- 1,000 lots at 101,000 (notional 101,000,000): taker 40,400, maker 12,625.
- 1,000 lots at 99,500 (notional 99,500,000): taker 39,800, maker `ceil(12,437.5)` = 12,438.
- Maker rebate −50 ppm on notional 99,500,000: `ceil(−4,975)` = −4,975 (a 4,975 rebate).
  On notional 12,345: `ceil(−0.61725)` = 0, so no rebate. Rebates round down in size.

### 7.3 Applying a position change

One function applies both fills and fund absorbs. It adds `d` lots (signed; buy > 0) whose
signed cost is `D` (for a fill, `D = d × P`) to a position `(pos, cost)` and returns the new
position and the realized PnL:

```
apply_change(pos, cost, d, D) -> (pos', cost', realized):
  if pos == 0 or sign(d) == sign(pos):            // opening or increasing
      return (pos + d, cost + D, 0)
  if abs(d) <= abs(pos):                          // reducing or closing
      removed = ceil_div(cost × abs(d), abs(pos))
      return (pos + d, cost − removed, −D − removed)
  // flipping: close all of pos, open the rest
  D_close = ceil_div(D × abs(pos), abs(d))
  return (pos + d, D − D_close, −D_close − cost)
```

For a fill, `D_close = sign(d) × abs(pos) × P` exactly (it divides evenly), so only the
reduce case rounds. On a fill, each slot then does `locked += realized − fee`, and sets
`key_dirty`.

**Why conservation holds exactly, whatever the rounding.** In all three cases,
`realized − (cost' − cost) = −D`. So a fill changes `locked − cost` by exactly
`−d × P − fee`. The counterparty's `d` has the opposite sign, so the two slots together
change `Σ locked − Σ cost` by `−(maker_fee + taker_fee)`, which `fees_collected` gains.
The rounding of `removed` only decides how the change is split between realized PnL
(which can be released) and the remaining cost basis (which cannot). Equity, and so every
margin state and liquidation key, is the same as with exact arithmetic.

Examples:
- Long 3 lots, cost 1,000 (average 333.33); sell 1 at 400. `removed = ceil(333.33) = 334`,
  realized = 400 − 334 = 66 (exact 66.67, rounded down). New `(2, 666)`.
- Short 3 lots, cost −1,000; buy 1 at 300. `removed = ceil(−333.33) = −333`, realized =
  −300 + 333 = 33 (exact 33.33, rounded down). New `(−2, −667)`.
- Flip: long 1,000, cost 100,000,000; sell 2,000 at 98,000 (`D = −196,000,000`).
  `D_close = −98,000,000`, realized = 98,000,000 − 100,000,000 = −2,000,000. New
  `(−1,000, −98,000,000)`: short 1,000 at 98,000.

Each slot's `pos` going to or from zero updates `market.nonzero_positions`.

### 7.4 Open totals

`open_buys` and `open_sells` always equal the remaining quantity of the account's resting
orders on each side in this market, at every command boundary (I4). Updates:
- place accepted: `+qty` on its side (6.1);
- replace accepted: `+(r' − r)`; shrink in place: `−(r − r')`; removal: `−r`;
- fill: `−q` on the maker's side for the maker and on the taker's side for the taker;
- `Cancelled` (user, self-trade, IOC remainder, size below filled, price band):
  `−remaining` on the cancelled order's side;
- liquidation: both set to 0.

In the modes without running totals (section 14) these fields are not kept; the same
numbers are recomputed from the book.

## 8. The post-command pass

Runs once at the end of every accepted `PlaceOrder`, `CancelOrder` and `ModifyOrder`, after
matching has finished and any GTC remainder rests; at the end of an accepted `SetLeverage`
on a slot with `W > 0` (for that one slot); and in `SetMark`, after the sweep, over the
owners of swept orders.

### 8.1 Touched slots

`touched` lists the slots the command changed, in this order: **the command's own slot
first, then maker slots in the order of their first fill**, each once. In `SetMark` it
lists the owners of swept orders, in the order of their first swept order. Deduplication is
O(1): a slot is appended only if `slot.touched_in != command_counter`, and then marked
(the counter rule is in 3.5). The fund is never in the list. Self-trade prevention means a
maker is never the taker's own account.

For each touched slot, in order:
1. If `equity < MM`: liquidate it (section 10.1).
2. Otherwise release (8.2).
3. Re-key it in the liquidation index (section 9.3; modes with the index only).

Liquidating one slot changes no other slot's equity (only the fund's), so the order only
decides the order of events. Release only happens when `equity > IM(W) >= MM`, so steps 1
and 2 never both apply. After a cancel or a sweep, step 1 never fires, because a cancel
doesn't change equity.

### 8.2 The release rule

```
release = max(0, min(locked, E) − IM(W))          // W, E at the current mark, after the command
locked −= release;  free += release               // and the Release events, only if release > 0
```

Properties (with `upnl = E − locked`):
- **Never below IM.** If `release > 0` then `release <= E − IM(W)`, so afterwards
  `E' >= IM(W)`.
- **Never releases unrealized profit.** Suppose something is released. If `upnl >= 0` the
  minimum is `locked`, so the slot keeps `locked' = IM(W)` and all of its unrealized
  profit. If `upnl < 0` the minimum is `E`, so `E' = IM(W)` and
  `locked' = IM(W) − upnl > IM(W)`. Either way `locked' >= IM(W)`: the requirement stays
  backed by money that was deposited or realized, and only money in `locked` (deposits,
  realized PnL, fees) ever reaches the free balance. If `locked` is negative, nothing is
  released.
- **When it releases.** Exactly when the slot holds more than it needs: after a cancel, a
  shrink or removal, a swept order, a reducing fill, a close (`IM(0) = 0`, so a flat slot
  with no orders releases everything), realized profit, an unfilled IOC remainder (which
  undoes that order's top-up), a tier commit that lowered its IM, or a mark that has moved
  in the slot's favour since it was last touched.
- **Safe with resting orders.** After a release `E >= IM(W)`, so claim 1 of 5.3 covers
  every order still resting; the sweep keeps them all inside the current band.
- No rounding beyond `IM`'s own.

Examples (long 1,000 lots, cost 100,000,000, locked 7,500,000, leverage 20; the account
cancels a resting buy of 500, so `W` falls from 1,500 to 1,000):
- Mark 102,000: `E = 9,500,000`, `IM(1,000) = 5,100,000`, `min(locked, E) = 7,500,000`,
  release 2,400,000. Afterwards `locked = 5,100,000`, `E = 7,100,000`: the 2,000,000 of
  unrealized profit stays in the slot.
- Mark 98,000: `E = 5,500,000`, `IM = 4,900,000`, `min = 5,500,000`, release 600,000.
  Afterwards `locked = 6,900,000`, `E = 4,900,000 = IM`.

## 9. The liquidation index

### 9.1 The key

A slot is liquidatable at mark `x` when `E(x) < MM(x)`:

```
E(x)  = locked + pos × x − cost
MM(x) = ceil_div(abs(pos) × x, 2 × Lmax)
```

For an integer `E` and a rational `y`, `E < ceil(y)` exactly when `E < y`. So the integer
check is exactly `2 × Lmax × E(x) < abs(pos) × x`, which is linear in `x`, and the boundary
tick comes out of one integer division, with no estimate to correct:

- **Long** (`pos > 0`): liquidatable exactly when `x < n / d` with
  `n = 2 × Lmax × (cost − locked)` and `d = pos × (2 × Lmax − 1)`. The key is the highest
  such tick:

  ```
  key_long = ceil_div(n, d) − 1;   no key if key_long < 1
  ```

  A long with no key (for example a fully collateralised 1x long) stays at or above MM at
  every positive price and is not indexed.
- **Short** (`pos < 0`, `s = −pos`): liquidatable exactly when `x > n / d` with
  `n = 2 × Lmax × (locked − cost)` and `d = s × (2 × Lmax + 1)`. The key is the lowest such
  tick, and always exists:

  ```
  key_short = floor_div(n, d) + 1
  ```

- **Flat** slots are not indexed. Their equity is `locked` at every mark.

"The key is at or through the mark" (`key_long >= mark`, or `key_short <= mark`) is then
exactly "`E(mark) < MM(mark)`", with no rounding mismatch. INFO.md asks to "compute the
rational estimate, then correct it by a tick with the integer check"; because the formula
is exact, that correction is kept only as debug assertions at every re-key:
- the boundary: for a long, `liquidatable(k)` and not `liquidatable(k + 1)`; for a short,
  `liquidatable(k)` and not `liquidatable(k − 1)`;
- the side of the mark: a slot is re-keyed only when it is not liquidatable at the current
  mark (the pass liquidates first), so `key_long < mark` and `key_short > mark`, which also
  gives `key_short >= 2`.

The property test checks the same thing against a binary search (14.2).

`n` and `d` are computed in `i128` (`abs(n) < 2^73`, `d < 2^70`; section 2.4).

### 9.2 Worked example (INFO.md's SP500 test)

A 20x long of 100,000 lots (1 unit) opened at mark 75,024 (7,502.4, tick 0.1) on a market
with `Lmax = 20`, fees 0. `cost = 7,502,400,000`, `locked = IM = 375,120,000` (5%).

```
n = 40 × (7,502,400,000 − 375,120,000) = 285,091,200,000
d = 100,000 × 39 = 3,900,000
n / d = 73,100.3077...    key_long = 73,101 − 1 = 73,100   (7,310.0)
```

Check: at 73,101, `E = 182,820,000 >= MM = 182,752,500` (not liquidated). At 73,100,
`E = 182,720,000 < MM = 182,750,000` (liquidated). INFO.md's `0.95 × 7,502.4 / 0.975 =
7,310.03` is the rational boundary.

The maker on the other side, short 100,000 at 75,024 with the same collateral:
`n = 40 × (375,120,000 + 7,502,400,000) = 315,100,800,000`, `d = 100,000 × 41`,
`n / d = 76,853.85`, `key_short = 76,854` (7,685.4).

Note on real data: Polymarket's live instrument list gives SP500 `max_leverage` 50, so
MMR is 1% and the same 20x long's key would be 71,992 (7,199.2). The FAQ lists SP500 at
20x, which is what the published 7,310.0 example assumes. Test T1 therefore uses a
market with `Lmax = 20` (section 16, C26).

### 9.3 Structure and re-keying

Per market, one indexed heap per side (D-003, D-018), behind a small type. (The spec first
chose two `BTreeSet`s; the M2 allocation rule of 14.4 switched it to D-018's fallback, see
18, P1.)

```
LiquidationIndex {
    longs:  indexed 4-ary heap of (Reverse<Price>, AccountId)   // first = highest key, then lowest AccountId
    shorts: indexed 4-ary heap of (Price, AccountId)            // first = lowest key, then lowest AccountId
    // each side: a Vec heap, plus a map from AccountId to its heap position
}
// methods: insert(side, key, account), remove(side, key, account),
//          refile(account, old, new), first_long(), first_short()
```

Ties go to the lower `AccountId` on both sides. Each slot remembers what it has in the
index (`indexed_key: Option<(Side, Price)>`), so re-keying is: if `key_dirty` is false,
stop (nothing the key depends on has changed); otherwise compute the key for the slot's
current `(pos, cost, locked)`, clear `key_dirty`, and if the key differs from
`indexed_key`, remove the old entry (if any) and insert the new one (if any). O(log n).

**When.** A key depends only on `pos`, `cost`, `locked` and `Lmax`. The first three change
through top-ups, fills, fees, releases and liquidation, all of which happen to touched
slots and set `key_dirty`, so every touched slot is re-keyed once, at the end of its
command (8.1), not after every fill. Leverage and tier changes move no key directly
(`SetLeverage` moves collateral and runs the same pass; a tier commit moves nothing).
`Lmax` can only change on an empty market. Keys are therefore exact at every command
boundary (invariant I10), which is all `SetMark` needs.

### 9.4 The SetMark walk

```
while first_long exists and its key >= mark:   liquidate that slot
while first_short exists and its key <= mark:  liquidate that slot
```

Liquidation removes the slot from the index, so each loop always looks at the current
first element. Cost: O((1 + L) × log n + orders cancelled) for `L` liquidations. Normally
only one side can be crossed (a slot already at or through the old mark would have been
liquidated earlier), but the order is fixed anyway: longs, then shorts. The sweep (5.4)
comes after the walk, so a liquidated account's orders are cancelled with reason
`Liquidation`, not `PriceBand`, and no collateral is released from a slot about to be
liquidated.

## 10. Liquidation and the insurance fund

### 10.1 Liquidating a slot

For slot `(a, m)` with `E < MM` (from the post-command pass or the `SetMark` walk):

1. **Cancel its orders.** `book.cancel_account(a, CancelReason::Liquidation)` into
   `scratch`; forward each `Cancelled` (oldest first). Set `open_buys = open_sells = 0`.
   **No release**: all `locked` stays in the slot for step 3, consistent with the equity
   the check used.
2. Emit `Liquidation { position: pos, account: a, market: m }`.
3. **Move everything to the fund.** Take `(p, c, l) = (pos, cost, locked)`; set the slot's
   `pos`, `cost` and `locked` to 0; if `p != 0`, `nonzero_positions −= 1`; remove it from
   the index (`indexed_key = None`); emit `PositionChanged(a, m)` (all zeros) and
   `InsuranceAbsorb { position: p, cost_basis: c, collateral: l, market: m }`.
4. **Fund netting.** `(fund_pos, fund_cost, r) = apply_change(fund_pos, fund_cost, p, c)`;
   `fund_balance += l + r`; if `fund_pos` went from 0 to non-zero,
   `nonzero_positions += 1`, and if from non-zero to 0, `−= 1`; recompute `fund_upnl` at
   the current mark and adjust `fund_upnl_total`. Emit `PositionChanged(FUND, m)` (with
   `locked` 0) and `BalanceChanged(FUND, fund_balance)`.

Economically this is a takeover at the bankruptcy price (D-002): the fund receives a
position whose equity is `E = l + p × mark − c`, which it gains (or, if negative, loses)
immediately. The trader's collateral for that position is gone; their free balance and
other slots are untouched.

**Conservation.** The slot's `locked − cost` goes from `l − c` to 0. By the same identity
as for fills (7.3), `fund_balance − fund_cost` changes by `l + r − (fund_cost' − fund_cost)
= l − c`. The totals are unchanged, whatever the rounding.

**A flat slot with negative `locked`.** Its equity is `locked < 0 = MM`, so the pass would
liquidate it like any other: `Liquidation { position: 0 }`, and the fund absorbs the
negative collateral (`fund_balance += l`). Commands can't produce one: a slot ends a
command flat only through fills that reduce its position (one command fills each slot on
one side only), and claim 2 of 5.3 shows those leave `E > 0`. The rule stays so that
invariant I5 has no exception; a unit test builds the state directly.

### 10.2 Fund netting examples

- **Reducing:** the fund is long 1,000 with cost 100,000,000 (average 100,000) and absorbs
  a short of 400 with cost −39,000,000 (average 97,500). `removed = 40,000,000`,
  `r = 39,000,000 − 40,000,000 = −1,000,000`: the fund effectively bought at 100,000 and
  sold at 97,500. New fund position `(600, 60,000,000)`.
- **Flipping:** the fund is long 300 with cost 30,000,000 and absorbs a short of 500 with
  cost −49,000,000 (average 98,000). `D_close = ceil_div(−49,000,000 × 300, 500) =
  −29,400,000`, `r = 29,400,000 − 30,000,000 = −600,000`, new fund position
  `(−200, −19,600,000)`, a short at 98,000.

The realized part goes to `fund_balance`; the fund's equity is unchanged by netting
itself (it only changes by the absorbed slot's equity).

### 10.3 Fund equity

```
fund_upnl(m)     = fund_pos × mark − fund_cost               // i128, per market, at its mark
fund_upnl_total  = Σ fund_upnl(m)                            // running total, i128
fund_equity      = fund_balance + fund_upnl_total
uncovered        = max(0, −fund_equity)                      // i128, then i64::try_from
```

`fund_upnl_total` is updated in O(1) on every `SetMark` (6.7) and every absorb.
`uncovered` is converted to `Micros` with `i64::try_from`; if it doesn't fit, the engine
panics naming it (the 2.4 policy).

### 10.4 The shortfall report

At the end of every command that can change the fund (a `Deposit` to `FUND`, `SetMark`,
and any place or modify whose pass liquidated something), the engine computes
`uncovered`. If it differs from `last_reported_uncovered`, it emits
`InsuranceShortfall { uncovered }` and stores it. So the event appears **at most once per
command, after all its other events**, whenever the value has changed (from 0 to positive,
between positive values, or back to 0). Values inside one command are not reported,
because a command is atomic.

Where bad debt comes from: an absorb with negative equity, which needs a mark move (a gap
past the bankruptcy price before the walk reached it, or claim 3 of 5.3), or a later
`SetMark` that moves the fund's position against it. There is no ADL in v1. The M3
load-test run reports the liquidation count, fund drawdown and peak `uncovered` in
`docs/BENCHMARKS.md`.

## 11. Event order reference

Building blocks:

```
TopUp(a, m)      = BalanceChanged(a), PositionChanged(a, m)          // only if need > 0
Release(a, m)    = PositionChanged(a, m), BalanceChanged(a)          // only if release > 0
FillBlock        = Fill, PositionChanged(maker), PositionChanged(taker)
Liquidate(a, m)  = Cancelled{Liquidation}*, Liquidation, PositionChanged(a, m),
                   InsuranceAbsorb, PositionChanged(FUND, m), BalanceChanged(FUND)
Pass             = for each touched slot in order: Liquidate | Release | nothing
Shortfall        = InsuranceShortfall, only if the uncovered amount changed
```

Money moves are reported source first, then destination: a top-up reports the free
balance, then the slot; a release the slot, then the free balance; a liquidation the slot,
then the fund.

| Command | Events when accepted |
|---|---|
| PlaceOrder | `Ack, [TopUp], (FillBlock or Cancelled{SelfTrade})*, [Cancelled{IocRemainder}], Pass, [Shortfall]` |
| ModifyOrder, replace | `Modified, [TopUp], (FillBlock or Cancelled{SelfTrade})*, Pass, [Shortfall]` |
| ModifyOrder, shrink in place | `Modified, Pass` |
| ModifyOrder, removal | `Cancelled{SizeBelowFilled}, Pass` |
| CancelOrder | `Cancelled{UserRequested}, Pass` |
| Deposit | `BalanceChanged, [Shortfall]` (the latter only for `FUND`) |
| Withdraw | `BalanceChanged` |
| SetLeverage | `LeverageSet, [TopUp or Release]` |
| SetMark | `MarkPrice, Liquidate*, Cancelled{PriceBand}*, Pass, [Shortfall]` (the pass here only releases) |
| SetMarketParams | `MarketParamsSet` |
| SetRiskTier | `RiskTierSet` |
| any rejected command | exactly one `Reject` |

`PositionChanged` always carries the slot's state after the change: `position`,
`cost_basis`, `locked`. `BalanceChanged` carries the new free balance (the fund balance for
`FUND`).

## 12. Invariants

A test harness checks all of these (the engine gets an `assert_invariants()` for tests;
cheap ones are `debug_assert!`s). Two kinds:
- **State invariants** (I1 to I12, I14 to I16) hold at every command boundary.
- **Command properties** (I13, I17 to I20) are checked from the snapshot before the
  command, the command and its events, with the test's own formulas (section 14.3).

I2 and I3 also hold inside a command after every event, except within a money block
(`TopUp`, `FillBlock`, `Release`, `Liquidate`), where one side of a transfer has been
reported and the other has not yet. The shadow ledger of 14.3 checks them after the last
event of each block and after every event outside a block. Sums are in `i128`.

- **I1. Not crossed.** For every market, `best_bid < best_ask` when both exist.
- **I2. Positions sum to zero.** For every market `m`:
  `Σ_a pos(a, m) + fund_pos(m) = 0`.
- **I3. Conservation.**
  `Σ_a free(a) + Σ_slots locked + fund_balance + Σ_m fees_collected(m)
   − Σ_slots cost − Σ_m fund_cost(m) = net_deposits`.
  Because positions sum to zero, this is "total equity at any mark equals net deposits".
- **I4. Open totals** (modes with running totals). For every slot, `open_buys`
  (`open_sells`) equals the sum of `remaining` over the account's resting buys (sells) in
  that market's book snapshot.
- **I5. Nothing left to liquidate.** For every non-fund slot in a market with a mark:
  `E >= MM`.
- **I6. Size bound.** For every non-fund slot: `abs(pos) <= W <= max_qty`.
- **I7. Idle flat slots hold nothing.** `pos == 0 and open_buys == 0 and open_sells == 0`
  implies `locked == 0`.
- **I8. Signs.** Free balances are `>= 0`. For every slot and fund position: `pos == 0`
  implies `cost == 0`; `pos > 0` implies `cost >= 0`; `pos < 0` implies `cost <= 0`. And
  `abs(cost) <= abs(pos) × max_price` for every slot (2.4).
- **I9. Fund totals.** `fund_upnl(m) = fund_pos(m) × mark(m) − fund_cost(m)` for every
  market with a mark, `fund_upnl_total = Σ fund_upnl(m)`, and
  `last_reported_uncovered = max(0, −(fund_balance + fund_upnl_total))`.
- **I10. Index exact** (modes with the index). The long set is exactly
  `{(key_long(s), a) : pos > 0 and key_long(s) exists}`, the short set exactly
  `{(key_short(s), a) : pos < 0}`, every `indexed_key` matches, and each key passes the
  boundary and side-of-the-mark checks of 9.1.
- **I11. Sequences.** Every resting order of account `a` has sequence `< next_seq(a)`, and
  `next_seq` never decreases.
- **I12. Liquidated means no orders.** After a command that emitted `Liquidation {a, m}`,
  account `a` has no resting orders in `m`, and its slot there is all zeros.
- **I13. Margin-call rule** (command property): each accepted place or replacing modify was
  strictly reducing, or `E + top-up >= IM(W')` at acceptance, so the slot was not left in
  margin call by an order that adds exposure. Under the literal alternative of Q1: strictly
  reducing, or `E >= IM(abs(pos))` before the top-up.
- **I14. Fees.** Every `Fill` has `maker_fee = ceil_div(P × q × maker_fee_ppm, 10^6)`,
  `taker_fee` likewise, and `maker_fee + taker_fee >= 0`.
- **I15. Rejects change nothing.** After a `Reject`, the engine snapshot (15.5) equals the
  one before the command.
- **I16. Orders inside the band.** In every market with a mark, every resting bid is at
  or below `upper` and every resting ask at or above `lower`.
- **I17. Top-up exact** (command property). If an accepted place, replacing modify or
  `SetLeverage` emitted a `TopUp` of `t` (the drop in the free balance), then `t >= 1` and
  `E_before + t == IM(W')` (for `SetLeverage`, `IM(W)` at the new leverage).
- **I18. Release bounded** (command property). After every `Release`, the slot has
  `locked >= IM(W)` and `E >= IM(W)`.
- **I19. Release complete** (command property). At the end of the command, every slot it
  touched (8.1) and did not liquidate has `min(locked, E) <= IM(W)`.
- **I20. Withdrawal rule** (command property). After an accepted `Withdraw`, recomputed
  over all of the account's slots in the snapshot (not from any engine list): `free >= 0`
  and `free + Σ locked >= ceil_div(Σ abs(pos) × mark, 10)`. For every `WithdrawalReserve`
  reject, the same rule with the amount taken out fails.

## 13. Unit tests

Accounts: A = 1, B = 2, C = 3, FUND = `AccountId::MAX`. `X#n` is order id
`order_id(X, n)`. Every market below is market 1 with `min_price` 1,000 (band rule 2 needs
at least 218 for these parameters) and a one-row tier table: each setup starts with
`SetMarketParams`, then `SetRiskTier { index 0, count 1, lower_bound 0, max_leverage }`
with the market's maximum. "1x" means the default leverage. Events are listed in full
where the test asserts them.

### T1. Liquidation boundary (INFO.md SP500 test)

Setup: `SetMarketParams { max_price 200,000, fees 0, band 20,000, max_leverage 20 }`;
Deposit FUND, A and B 1,000,000,000 each; SetLeverage A and B 20; SetMark 75,024.

1. `B#1 sell 100,000 @ 75,024 GTC` → `Ack, BalanceChanged(B, 624,880,000),
   PositionChanged(B: 0, 0, 375,120,000)`.
2. `A#1 buy 100,000 @ 75,024 GTC` → `Ack, BalanceChanged(A, 624,880,000),
   PositionChanged(A: 0, 0, 375,120,000), Fill{B#1, A#1, 75,024, 100,000, fees 0, 0, Buy},
   PositionChanged(B: −100,000, −7,502,400,000, 375,120,000),
   PositionChanged(A: 100,000, 7,502,400,000, 375,120,000)`.
   A's key is 73,100 (long); B's is 76,854 (short).
3. `SetMark 73,101` → `MarkPrice` only.
4. `SetMark 73,100` → `MarkPrice, Liquidation{A, 100,000}, PositionChanged(A: 0, 0, 0),
   InsuranceAbsorb{100,000, 7,502,400,000, 375,120,000},
   PositionChanged(FUND: 100,000, 7,502,400,000, 0), BalanceChanged(FUND, 1,375,120,000)`.
   Fund equity = 1,375,120,000 + (7,310,000,000 − 7,502,400,000) = 1,182,720,000: it gained
   A's remaining equity of 182,720,000. No `InsuranceShortfall`.

### T2. Withdrawal reserve ($10 on a $100 position at 20x)

Setup: `max_price 1,000,000, fees 0, band 20,000, max_leverage 20`; Deposit A 20,000,000,
B 100,000,000; SetLeverage A 20; SetMark 100,000. (1,000 lots at 100,000 = $100.)

1. `B#1 sell 1,000 @ 100,000` (B at 1x: top-up 100,000,000, free 0).
2. `A#1 buy 1,000 @ 100,000`: top-up 5,000,000 ($5 IM), free 15,000,000; fill.
3. `Withdraw A 10,000,001` → `Reject WithdrawalReserve` (4,999,999 + 5,000,000 <
   10,000,000).
4. `Withdraw A 10,000,000` → `BalanceChanged(A, 5,000,000)`.
5. `Withdraw A 1` → `Reject WithdrawalReserve`.

### T3. A margin-called slot with zero free balance can close, and it fills

Setup: `max_price 1,000,000, maker 125, taker 400, band 20,000, max_leverage 20`; Deposit
A 5,000,000, B and C 1,000,000,000; SetLeverage A 20; SetMark 100,000.

1. `B#1 sell 1,000 @ 101,000` (top-up 100,000,000 at 1x).
2. `A#1 buy 1,000 @ 101,000` (inside the band, upper 102,000): top-up 5,000,000, free 0.
   `Fill{B#1, A#1, 101,000, 1,000, maker_fee 12,625, taker_fee 40,400, Buy}`;
   B `(−1,000, −101,000,000, 99,987,375)`, A `(1,000, 101,000,000, 4,959,600)`.
   A now: `E = 3,959,600`, `IM = 5,000,000`, `MM = 2,500,000`: margin call, free 0.
3. `A#2 buy 10 @ 100,000` → `Reject MarginCall` (`need = 1,090,400 > 0 = free`).
4. `C#1 buy 1,000 @ 99,500` (top-up 100,000,000 at 1x).
5. `A#3 sell 1,000 @ 99,500` is strictly reducing (`open_sells' = 1,000 <= pos`), so no
   margin check → `Ack, Fill{C#1, A#3, 99,500, 1,000, maker_fee 12,438, taker_fee 39,800,
   Sell}, PositionChanged(C: 1,000, 99,500,000, 99,987,562), PositionChanged(A: 0, 0,
   3,419,800), PositionChanged(A: 0, 0, 0), BalanceChanged(A, 3,419,800)`.
   A's realized loss is 1,500,000 and its fees 80,200, all taken from `locked`
   (5,000,000 − 40,400 − 1,500,000 − 39,800 = 3,419,800), which is then released.
   `fees_collected` = 105,263.

### T4. A taker whose own flip at the band edge lands it between zero and MM

Setup: `max_price 1,000,000, fees 0, band 20,000, max_leverage 20`; Deposit FUND
1,000,000,000, A 5,500,000, B and C 1,000,000,000; SetLeverage A 20; SetMark 100,000.

1. `B#1 sell 1,000 @ 100,000`; `A#1 buy 1,000 @ 100,000`: A `(1,000, 100,000,000,
   5,000,000)`, free 500,000.
2. `A#2 buy 100 @ 97,000 GTC` (rests): `W' = 1,100`, `IM = 5,500,000`, top-up 500,000,
   free 0.
3. `C#1 buy 2,000 @ 98,000 GTC` (rests).
4. `A#3 sell 2,000 @ 98,000 IOC` (the band's lower edge). Not strictly reducing; `W' =
   max(1,100, 1,000) = 1,100`, `need = 0`, accepted. Events:
   `Ack, Fill{C#1, A#3, 98,000, 2,000, fees 0, 0, Sell}, PositionChanged(C: 2,000,
   196,000,000, 200,000,000), PositionChanged(A: −1,000, −98,000,000, 3,500,000)`, then
   the pass liquidates A (`E = 1,500,000 < MM = 2,500,000`): `Cancelled{A#2, 100, Buy,
   Liquidation}, Liquidation{A, −1,000}, PositionChanged(A: 0, 0, 0),
   InsuranceAbsorb{−1,000, −98,000,000, 3,500,000},
   PositionChanged(FUND: −1,000, −98,000,000, 0), BalanceChanged(FUND, 1,003,500,000)`.
   C is not released (`min(locked, E) = IM`).
   Checks: A has no resting orders; fund equity = 1,003,500,000 − 2,000,000 =
   1,001,500,000, up by A's non-negative equity 1,500,000; no `InsuranceShortfall`.

### T5. Collusion attempt is rejected and fund equity is unchanged

Setup: `max_price 1,000,000, fees 0, band 20,000, max_leverage 20`; Deposit FUND
1,000,000,000, A 5,000,000, C 1,000,000,000; SetLeverage A 20; SetMark 100,000.

1. `C#1 sell 1,000 @ 102,000`; `A#1 buy 1,000 @ 102,000` (the band's upper edge): A
   `(1,000, 102,000,000, 5,000,000)`, free 0. `E = 3,000,000`, `IM = 5,000,000`,
   `MM = 2,500,000`: margin call (key 99,487).
2. `C#2 buy 2,000 @ 98,000` (rests; C needs no top-up).
3. `A#2 sell 2,000 @ 98,000` → `Reject MarginCall` (`need = 2,000,000 > 0`).
   Fund equity is 1,000,000,000 before and after.

Why it matters: had the flip been accepted without a margin check, A would be short 1,000
at 98,000 with `locked = 1,000,000` and equity −1,000,000 at the mark; the fund would
absorb that bad debt, and C would pocket A's 5,000,000 plus the fund's 1,000,000.

### T6. A stale order is swept when the mark moves (review F1)

Setup: `max_price 1,000,000, fees 0, band 20,000, max_leverage 20`; Deposit FUND
1,000,000,000, A 5,000,000, B 1,000,000,000; SetLeverage A 20; SetMark 100,000.

1. `A#1 buy 1,000 @ 100,000 GTC` (the book is empty, so it rests) → `Ack,
   BalanceChanged(A, 0), PositionChanged(A: 0, 0, 5,000,000)`.
2. `A#2 sell 1,000 @ 100,001 GTC` → `Ack`. `W' = max(1,000, 1,000) = 1,000`, `need = 0`:
   one IM backs both sides.
3. `SetMark 98,040` → `MarkPrice` only. `upper = floor(98,040 × 1.02) = 100,000`, so A#1
   is exactly at the edge and stays.
4. `SetMark 98,039` → `MarkPrice, Cancelled{A#1, 1,000, Buy, PriceBand},
   PositionChanged(A: 0, 0, 4,901,950), BalanceChanged(A, 98,050)`. `upper` = 99,999 <
   100,000. A#2 stays (100,001 >= `lower` = 96,079), so `W` is still 1,000 and the release
   is `5,000,000 − IM(1,000) = 5,000,000 − 4,901,950 = 98,050`.
5. `B#1 sell 1,000 @ 100,000` → `Ack, BalanceChanged(B, 901,961,000),
   PositionChanged(B: 0, 0, 98,039,000)`. Nothing to fill; B#1 rests.
   Fund equity is 1,000,000,000 throughout.

Why it matters: without the sweep, A#1 would still rest at 100,000. Had the mark drifted on
to 80,000 in 1% steps before B's sell, B#1 would fill it, A would be liquidated at equity
−15,000,000, and the fund would lose 15,000,000 while B held a 20,000,000 profit (5.4).

### Further unit tests (one each)

- `Duplicate`: a rejected order does not advance `next_seq` and can be resent; an accepted
  one blocks lower and equal sequences in every market.
- Validation order: an order failing several checks gets the first reason in 6.1; no
  state changes (I15).
- Orders from `FUND` → `ReservedAccount`; `Withdraw` and `SetLeverage` for `FUND` too.
- Unknown account: its order → `InsufficientMargin`, and no account is created.
- `NoMark` before the first `SetMark`, and again after `SetMarketParams`; `NoRiskTiers` for
  a `SetMark` before the tier table is committed.
- `Reject.account` is `AccountId::MAX` for a rejected `SetMark`, `SetMarketParams` or
  `SetRiskTier`.
- Band edges: at mark 75,024, band 8,600 ppm: buy 75,669 accepted, 75,670 rejected; sell
  74,379 accepted, 74,378 rejected.
- Band rules at `SetMarketParams`: at `Lmax` 50 and taker fee 400, band 8,600 is accepted
  and 8,601 rejected (rule 1); with band 8,600, `min_price` 1,004 is accepted and 1,003
  rejected (rule 2); `Lmax` 1 with band 225,000 and taker fee 225,000 passes rule 1 and is
  rejected by rule 2.
- The sweep, H1's sequence (review; `Lmax` 20, fees 400/125, band 22,100): at mark
  100,000, A (20,100,000 deposited, 20x) buys 1,000 at 100,000 from B and rests a buy of
  3,000 at 80,000; `SetMark 90,000`; A rests a strictly reducing sell of 1,000 at 88,011
  (the lower edge); `SetMark 100,000` → `MarkPrice, Cancelled{A#3, 1,000, Sell, PriceBand}`
  (no release: `W` is still 4,000). A cancels the 80,000 buy (release 15,000,000); C's buy
  of 1,000 at 100,000 then finds nothing to fill. Fund equity unchanged (without the sweep
  it falls by 7,000,002).
- Sweep order: several bids above the new `upper` at two prices, two of them at the same
  price, are cancelled highest price first, oldest first; each owner is released once.
- Residual risk pinned (5.3, claim 3): the F3 sequence (SP500's live parameters,
  `min_price` 1,004) ends with the fund's equity down exactly 224,015,813 and
  `fees_collected` 21,594,773. If a later milestone adds the margin-call index, this test
  changes on purpose.
- `SizeLimit`: `W'` one above `max_qty` is rejected; a quantity near `i64::MAX` does not
  overflow.
- Tier boundary: the IM examples of 4.4, plus, at mark 100,000 with SP500's tiers:
  5,000,000 lots (exactly $500,000) with chosen leverage 50 → 25x, IM 20,000,000,000;
  4,999,999 lots → 50x, IM 9,999,998,000; 5,000,000 lots with chosen leverage 20 →
  `lev_eff` 20 (the chosen leverage binds, not the tier), IM 25,000,000,000.
- Tier staging: SP500's 8 rows commit only at index 7; an order placed after row 3 is
  margined with the old table; a commit is accepted on a market with positions and
  orders, and the next check of a slot uses the new table; a row out of order (index 2
  right after index 0) is rejected `InvalidParams` with staging unchanged; a new row 0
  restarts the batch.
- Fees: the rounding examples of 7.2, including a rebate rounded to 0.
- Removed-cost rounding: the long and short examples of 7.3.
- Release: both examples of 8.2; an unfilled IOC's top-up is released in the same command.
- Modify: removal (`SizeBelowFilled`) and shrink in place release collateral in a margin
  call; a replace in margin call that is strictly reducing is accepted.
- `SetLeverage`: a raise releases, a cut tops up, a cut that the free balance can't cover
  is rejected and leaves the leverage unchanged.
- `SetMarketParams` on a market with a resting order or a position (the fund's included) →
  `MarketNotEmpty`; each `InvalidParams` condition.
- Fund netting: both examples of 10.2, through two liquidations of opposite positions.
- Shortfall: a mark gap past a long's bankruptcy price emits `InsuranceShortfall` with the
  exact uncovered amount; a later `Deposit` to `FUND` that covers it emits
  `InsuranceShortfall { 0 }`.
- The flat negative-collateral liquidation (10.1), with the slot's state set by a
  `#[cfg(test)]` helper, since commands can't reach it.

## 14. Performance, ablations and the Mode seam

### 14.1 Cost of the hot path

A place with risk on adds to the book's work: a market lookup by index, hash lookups of
the account and the slot (12 in all for a place that rests with a top-up, 3 more per fill;
each repeat costs about 6 to 11 ns, see `engine.rs` and 18, P2), a handful of `i64`
multiplications, two `i128` fee products per fill (maker and taker), a tier scan of at
most 8 rows, and at the end one re-key per touched slot whose key inputs
changed (one `i128` division and O(log n)). Nothing loops over orders or positions, with
two stated exceptions:
- a liquidation inside a place's pass cancels that account's `k` orders in the market
  (O(k)); `k` is not bounded in v1, because Polymarket's 200-order limit is not enforced
  (C23);
- `SetMark`'s sweep costs O(1 + orders swept) (5.4).

Target: **~1µs per risk-checked order** (M2 definition of done).

### 14.2 The Mode seam

The engine is generic over its book and a `Mode` with two compile-time switches:

```rust
pub trait Mode {
    /// true: open_buys/open_sells are running totals on the slot (O(1)).
    /// false: they are recomputed by walking the account's resting orders in the book (O(k)).
    const RUNNING_TOTALS: bool;
    /// true: SetMark walks the liquidation index (section 9).
    /// false: SetMark scans every slot in the market.
    const LIQUIDATION_INDEX: bool;
}
pub struct Fast;              // true,  true   production
pub struct Naive;             // false, false  the reference
pub struct NaiveTotals;       // false, true   ablation A
pub struct NaiveLiquidation;  // true,  false  ablation B
```

The constants are read in exactly four engine functions: `open_totals(slot)` (fields, or
`book.open_quantities(account)`), `add_open(slot, side, delta)` (update, or nothing),
`rekey(slot)` (index, or nothing), and the `SetMark` liquidation step. They are constants,
so the unused branch compiles away and costs nothing in `Fast`. Everything else, including
the sweep and `market.accounts`, is the same in every mode.

**Naive `SetMark`:** collect every account in `market.accounts` (creation order) whose slot
has `pos != 0` and `E < MM` at the new mark. For each, find its boundary tick by **binary
search** with the integer check (not with the closed form of 9.1), on a bounded interval.
Every slot was safe at the old mark (I5 at the previous command boundary), so a long's key
lies in `[new_mark, old_mark − 1]` and a short's in `[old_mark + 1, new_mark]`. The search
asserts `liquidatable(new_mark)` and not `liquidatable(old_mark)`, costs
O(log |old − new|) and never leaves the price range. If the market had no mark before, no
slot can have a position (positions need a mark), which the naive step asserts. Sort longs
by (key descending, `AccountId` ascending) and shorts by (key ascending, `AccountId`
ascending), and liquidate longs, then shorts. That is the same set and the same order as
the index walk, derived without the index or its formula, so agreement checks both.

Like `ReferenceBook` in M1, `Engine<ReferenceBook, Naive>` is the executable reference for
`Engine<Book, Fast>`.

### 14.3 Property test

`engine/tests/engine_equivalence.rs`: random command streams applied to
`Engine<Book, Fast>`, `Engine<ReferenceBook, Naive>`, and `Engine<Book, M>` for all four
modes (cheap). After every command all engines must have emitted identical events and have
identical snapshots (15.5), and `assert_invariants` must pass on each; I4 is checked only
where `RUNNING_TOTALS` is on and I10 only where `LIQUIDATION_INDEX` is on.

**What the equivalence proves, and what it doesn't.** All modes share every money formula
(IM with tiers, MM, the top-up, the release, `apply_change`, fees, fund netting); they
differ only in the four `Mode` functions. So equal events prove two things: the running
totals equal the book's, and the index walk picks the same slots in the same order as a
scan. A wrong formula gives the same wrong events in every mode. Three more checks cover
that:
- **An independent checker** in the test (about 60 lines, with its own IM through a
  separately written tier lookup, MM, equity and `strictly_reducing`). From the snapshot
  before each command, the command and its events, it checks I5, I13 and I17 to I20.
- **A shadow ledger** built only from events: `PositionChanged` gives pos, cost and locked;
  `BalanceChanged` gives free or `fund_balance`; `Fill` gives the fees; the commands give
  `net_deposits`. It checks I2 and I3 after each event block, and at the end of each
  command it must equal the snapshot. That also proves the event stream alone rebuilds
  every balance and position, which is why `PositionChanged` carries `locked` (M3 replay
  and private feeds).
- **The unit tests** of section 13, whose numbers were worked out by hand and in the
  throwaway model.

**Evidence it bites** (recorded in D-017 when implemented, as D-009 did). Each of these
planted bugs, one at a time, must fail the property: a key off by one tick; ties going to
the higher `AccountId`; no subtraction on a `SelfTrade` cancel; release computed as
`locked − IM(W)`; a top-up one micro too large; the `SetMark` sweep skipped.

**Generator** (tuned like D-009): 2 markets (a 20x market with fees 400/125 ppm and a 50x
market with SP500's real tiers), 6 accounts plus the fund, 256 scenarios of up to 300
commands; deposits, withdrawals (some at the reserve edge), `SetLeverage`, `SetMark` as a
random walk with small steps (so stale orders get swept) and occasional 5-15% jumps (so
liquidations and shortfall happen), places, cancels and modifies as in the book test, with
prices mostly near the band edges; plus invalid commands (fund orders, reused sequences,
zero amounts, out-of-band prices). Two additions so that rare rules run:
- a **twin step**: two accounts with the same deposit and leverage place identical orders
  against a third, so they get identical keys and the `(key, AccountId)` tie rule decides;
- on the tiered market, sizes drawn around the tier bounds, with deposits that can reach
  them.

The run prints a **coverage table** and fails if any row is below a set minimum: each
reject reason, top-up, release, a strictly reducing order accepted in margin call, a pass
liquidation of the own slot and of a maker, `SetMark` liquidations of longs and of shorts,
same-key ties, swept bids and swept asks, shortfall rising and back to 0, fund netting
reduce and flip, a tier crossing, a tier commit on a live market, an IOC top-up released,
and `SetLeverage` top-up, release and reject.

**Engine seed and capacity test** (the engine's version of the book's
`capacity_and_hash_seed_change_nothing_but_speed`): scratch capacity 0 and 1, touched
capacity 0, two hash seeds; the events must be identical.

### 14.4 Ablations and microbenchmarks (M2 definition of done)

- **A. O(1) check vs a naive loop** (`Fast` vs `NaiveTotals`): time per margin-checked
  place that rests without filling, and per cancel, with resting orders per account
  `k` in {1, 16, 256, 4,096} and positions per market `n` in {100, 10,000, 1,000,000}.
  Expected: `Fast` flat in `k` and growing like `log n` (the re-key); naive linear in `k`.
- **B. Liquidation index vs full rescan** (`Fast` vs `NaiveLiquidation`): time per
  `SetMark` and slots examined, for `n` in {1,000, 100,000, 1,000,000} positions and marks
  that liquidate 0, 1 and 100 of them; and time per order in the same books, so the trade
  is shown both ways (O(log n) per order against O(n) per mark).
- The risk layer's cost: place with risk on vs the book alone, same flow.

Results go to `docs/BENCHMARKS.md` with machine and commit. Benchmarks never clone an
engine (a cloned `Vec` loses its spare capacity, D-010).

**Allocation** (`engine/tests/no_alloc.rs`, engine cases):
1. Warm up first: create every account and slot of the flow, so map inserts and
   `market.accounts` pushes are done before counting.
2. Run the flow on `Engine<Book, NaiveLiquidation>` (running totals, no index) with only
   `SetMark`s that liquidate nothing (the naive step collects and sorts). Assert 0
   allocations and 0 frees.
3. Run the same flow on `Engine<Book, Fast>` and report allocations plus frees per 10,000
   commands. The difference from step 2 is exactly the index's share (std's B-tree
   allocates when a node splits and frees when nodes merge).
4. If `Fast` allocates more than once per 1,000 commands in that flow, or the allocations
   show in the p99 of a risk-checked place, switch the index to the indexed binary heap
   (D-018); `LiquidationIndex` (9.3) keeps that change local.

## 15. Type and trait changes

### 15.1 Events (`Event` stays at 64 bytes)

Fields go largest first (D-005), and each new field goes last:

| Event | Fields, in this order | Size |
|---|---|---|
| `Fill` | `maker_order, taker_order, price, qty, maker_fee, taker_fee, market, taker_side` | 56 |
| `Cancelled` | `order_id, remaining, market, reason, side` | 24 |
| `PositionChanged` | `position, cost_basis, locked, account, market` | 32 |
| new `LeverageSet` | `account, market, leverage` | 8 |
| new `MarketParamsSet` | the `SetMarketParams` command | 32 |
| new `RiskTierSet` | the `SetRiskTier` command (`lower_bound, market, max_leverage, index, count`) | 16 |

The order matters: with `taker_side` first, `Fill` would be 64 bytes and `Event` 72. Each
changed struct gets its own compile-time assert, as `book.rs` does for its order slot, for
example `const _: () = assert!(size_of::<Fill>() == 56);`, so a reordered field fails at
that struct. The largest event is still `Fill`, so `size_of::<Event>() == 64`. Both books
must emit the new `Fill` and `Cancelled` fields, and the book equivalence tests compare
them. `Fill`'s doc comment ("fees are filled in by the ledger") stays true.

Why: `taker_side` and `side` let the ledger and consumers update open totals and know who
bought from the event alone; `locked` in `PositionChanged` makes collateral part of the
replayable state; the three echo events record accepted operator commands.

### 15.2 Reject and cancel reasons

New reject reasons: `ReservedAccount` (the fund's id used for orders, withdrawals or
leverage), `SizeLimit` (`W'` above `max_qty`), `InvalidAmount` (deposit or withdrawal
amount below 1, or a deposit that would overflow), `InvalidParams` (`SetMarketParams` /
`SetRiskTier` validation), `WithdrawalReserve` (the 10% rule), `NoRiskTiers` (`SetMark`
before the market has a tier table). Docs updated: `Duplicate` (now also the engine's
per-account sequence rule), `InsufficientBalance` (free balance below the amount).
Existing and used as is: `UnknownMarket`, `UnknownOrder`, `NoMark`, `PriceBand`,
`MarginCall`, `InsufficientMargin`, `MarketNotEmpty`, `InvalidLeverage`.

New cancel reason: `PriceBand` (the `SetMark` sweep). `Liquidation` exists.

### 15.3 Commands

`SetRiskTier` gains `count: u8` after `index` (14 bytes of fields, still 16 with
padding). `Command` stays at 40 bytes.

### 15.4 OrderBook trait additions

```rust
/// Builds an empty book (the engine creates books from SetMarketParams).
fn with_config(config: BookConfig, options: BookOptions) -> Self where Self: Sized;

/// One resting order, or None. O(1) in Book (id map); a scan in ReferenceBook.
fn order(&self, order_id: OrderId) -> Option<RestingOrder>;   // RestingOrder gains `side`

/// Remaining quantity of `account`'s resting buys and sells in this book, by walking its
/// per-account list. O(k), no allocation. Used only by the modes without running totals.
fn open_quantities(&self, account: AccountId) -> (Qty, Qty);

/// Cancels every bid priced above `limit` (side Buy) or every ask priced below `limit`
/// (side Sell), best price first and oldest first within a level, with a
/// `Cancelled { reason }` for each. O(1 + cancelled) in Book (it walks from the best
/// level inward using the level index); a filter and sort in ReferenceBook.
fn cancel_beyond(&mut self, side: Side, limit: Price, reason: CancelReason, events: &mut impl EventSink);
```

The post-only pre-check uses the existing `best_bid`/`best_ask`; emptiness uses both being
`None`. The book equivalence test gets a `cancel_beyond` step.

### 15.5 Engine API and snapshot

```rust
pub struct EngineOptions {
    pub order_capacity: usize,     // per market book (BookOptions)
    pub id_hash_seed: u64,         // for the books and the engine's maps (D-011)
    pub scratch_capacity: usize,   // also the capacity of `touched` (3.5)
    pub account_capacity: usize,
    pub slot_capacity: usize,      // per market
}
impl<B: OrderBook, M: Mode> Engine<B, M> {
    pub fn new(options: EngineOptions) -> Self;
    pub fn apply(&mut self, command: &Command, events: &mut impl EventSink);
    pub fn assert_invariants(&self);             // tests; walks everything
    pub fn snapshot(&self) -> EngineSnapshot;    // tests; allocates
}
pub const FUND: AccountId = AccountId::MAX;
```

`EngineSnapshot` holds, in a fixed order (sorted `Vec`s or `BTreeMap`s, never hash-map
order):
- **Accounts**, sorted by id: `free`, `next_seq`. (An account's existence is part of the
  snapshot, so I15 catches a rejected command that creates one.)
- **Markets**, sorted by id: the parameters, `max_qty`, the live tier table, the staged
  rows with `staged_rows` and `staged_count`, `mark`, `upper`, `lower`, `fund_pos`,
  `fund_cost`, `fund_upnl`, `fees_collected`, `nonzero_positions`, `accounts` (creation
  order), the book snapshot, and the **slots**, sorted by account: `pos`, `cost`, `locked`,
  `leverage`, and `open_buys`/`open_sells` read through `open_totals()` (never the raw
  fields, which the naive modes don't keep).
- **Engine-wide:** `fund_balance`, `fund_upnl_total`, `last_reported_uncovered`,
  `net_deposits`.

Left out, because they differ between modes or are bookkeeping: `indexed_key`,
`key_dirty`, `touched_in`, `command_counter`, the `LiquidationIndex` sets (I10 checks them
in the modes that have them), `scratch` and `touched`.

A `#[cfg(test)]` helper sets one slot's `(pos, cost, locked)` for the flat
negative-collateral test.

## 16. Choices made in this spec

Each is a place where INFO.md left room. The owner may want to review them; the three that
are product decisions are repeated in section 17.

- **C1. Margin state uses `abs(pos)`, the check uses `W'`.** Margin call is
  `E < IM(abs(pos))` (INFO.md: "based on the position's size at mark"); the pre-trade
  requirement is `IM(W')`.
- **C2. Tier by notional at the mark** of the size being margined (`W' × M` in the check),
  applied to the whole notional; a notional exactly at a bound is in the higher tier.
- **C3. MM is flat:** `ceil_div(abs(pos) × M, 2 × Lmax)`, no tiers, independent of chosen
  leverage (FAQ).
- **C4. Rounding** as in 2.3: requirements up, fees toward +infinity (rebates down in
  size), band edges toward the mark (stored per mark), removed cost basis up (realized
  down), keys exact.
- **C5. The order's own fee is not reserved** in the check. The band rules already cover
  fees for solvency. Consequence: a position opened with a just-enough top-up sits below
  IM by its fee right after the fill (see Q1).
- **C6. Realized PnL and fees stay in `locked`;** the release rule runs at the end of every
  command for every touched slot. On `SetMark` it runs only for the owners of swept orders.
- **C7. Margin call is cured by a top-up** when the free balance allows (Q1).
- **C8. Validation order:** the book's checks first, the cancel's included (so a top-up is
  never followed by a book reject), then `NoMark`, band, size, margin. `Duplicate` sits
  where the book has it. The book's first event is checked with `assert!`.
- **C9. `next_seq` advances only on accepted places;** rejected ids may be resent (M3's
  nonce must be used up by rejected messages too).
- **C10. No special reason for accounts without a deposit:** they get
  `InsufficientMargin`; rejects create nothing.
- **C11. The fund is `AccountId::MAX`, capitalised by `Deposit`, cannot trade, withdraw or
  set leverage** (Q2), and has no `Account` entry.
- **C12. Two band rules at `SetMarketParams`:** rule 1,
  `20 × Lmax × (band + max(taker, maker fee)) <= 9,000,000` (the owner's rule with a 10%
  margin; the largest allowed band is the recommended value for the load generator), and
  rule 2, the exact condition with the `fee × band` term and fee rounding, which sets a
  floor on `min_price` of about `20 × Lmax` ticks.
- **C13. The band applies to strictly reducing orders too,** and resting orders that end
  up outside the band after a mark move are cancelled by the sweep (Q3).
- **C14. `SetMark` must be inside the market's price range.**
- **C15. Market lifecycle and tiers.** A market opens with `SetMarketParams`, then a tier
  table, then `SetMark`; `SetMark` without a table is `NoRiskTiers`. A tier table is
  staged row by row and committed in one step on its last row, also on a live market,
  because tiers change only IM. `SetMarketParams` needs an empty market (no resting orders,
  no positions, the fund's included) and clears the mark and the tier table. Any client
  can keep a market non-empty with a 1-lot order or position, and a fund position keeps it
  so for good; changing fees or the band live, and an operator cancel, are "Later".
- **C16. `SetLeverage` above the maximum is rejected** (`InvalidLeverage`, as the existing
  enum says), not capped. It recomputes `IM(W)`: tops up, releases, or rejects
  `InsufficientMargin`; never `MarginCall`.
- **C17. Keys are exact by one division;** INFO.md's "correct by a tick" becomes debug
  assertions (the boundary, and `key_long < mark < key_short` after a re-key).
- **C18. Walk order:** longs (highest key first), then shorts (lowest first), ties by lower
  `AccountId`.
- **C19. The post-command pass uses the direct check `E < MM`,** which equals the key test
  and also covers flat slots with negative collateral.
- **C20. Fund netting uses the fills' `apply_change`;** the fund's realized PnL goes to its
  balance.
- **C21. `InsuranceShortfall` at most once per command,** at its end, when the value changed.
- **C22. No liquidation fee.** Polymarket charges 0.5% (`liquidation_fee`); D-002's
  takeover at the bankruptcy price has none. Adding one is "Later".
- **C23. Polymarket's per-order limits are not enforced** (`min_notional` $10,
  `max_order_count` 200, `max_market_notional`, `max_limit_notional`). Only the overflow
  bound `SizeLimit` exists.
- **C24. Overflow policy:** `PRICE_LIMIT = 2^32`, `NOTIONAL_LIMIT = 2^53`, reject
  `SizeLimit`; per-slot bounds `abs(cost) <= abs(pos) × max_price`, `locked < 2^55` at
  command boundaries, equity below `2^56`; the `i64`/`i128` split of 2.4; aggregates
  checked, panic on overflow (never wrap).
- **C25. Open totals count an order in full before matching,** IOC included; so an IOC is
  margined as if it could rest, and its unused top-up is released at the end.
- **C26. The SP500 test uses `Lmax = 20`** (the FAQ and INFO.md), not the live list's 50,
  under which the same position's key would be 71,992 (7,199.2). The tick of 0.1 is
  confirmed by the live list.
- **C27. Event and command additions** (15): `locked` in `PositionChanged`, sides in
  `Fill` and `Cancelled` (new fields last), three echo events, `count` in `SetRiskTier`,
  cancel reason `PriceBand`, reject reason `NoRiskTiers`; `Reject.account` is
  `AccountId::MAX` for market-level commands. Money moves are reported source first.
- **C28. Scratch-buffer post-processing** of the book's events rather than a sink that
  updates the ledger while the book is matching (D-016); scratch and touched are walked by
  index.
- **C29. An indexed 4-ary heap per side for the index,** behind a small
  `LiquidationIndex` type. The spec first chose `BTreeSet`, with the heap as the fallback
  if `Fast` allocated more than once per 1,000 commands in the M2 allocation test; it
  allocated 47 times per 1,000, so the fallback was taken (18, P1; D-018).
- **C30. The withdrawal rule counts `free + locked`,** ignoring unrealized PnL both ways,
  as INFO.md states it.
- **C31. The `SetMark` sweep** (5.4): after the walk, cancel bids above `upper` and asks
  below `lower` (bids first; best price first, oldest first), then release their owners.
  It keeps every resting order inside the current band (I16), which the band argument and
  the release rule rely on (Q3).
- **C32. Residual risk accepted:** orders that grow a position can create bad debt after an
  adverse mark move, bounded by about `band + fee` of their filled notional (5.3, claim 3).
  The second index that would close it is INFO.md's "Later" margin-call index.
- **C33. Engine layout:** markets in a `Vec` indexed by id; slots inside their market in a
  seeded `IdMap`, created only by `slot_or_create`, which also appends to
  `market.accounts` (kept in every mode); no per-account list of markets (`Withdraw` loops
  over markets); `command_counter` moves by one per accepted command; `key_dirty` skips
  unneeded key divisions.
- **C34. What the tests compare:** the snapshot is defined field by field (15.5); I2 and I3
  are checked per event block by a shadow ledger built from events, not per single event;
  an independent checker with its own formulas checks the command properties I13 and I17
  to I20.
- **C35. Naive `SetMark` search** is a plain binary search between the old and the new
  mark, never outside the price range.
- **C36. Emission rules:** `TopUp` only if `need > 0`, `Release` only if `release > 0`;
  `uncovered` is converted with `i64::try_from` (panic if it doesn't fit).

## 17. Questions for the owner

**Answered by the owner on 2026-09-29: Q1 yes (top up and accept), Q2 no (the fund does
not trade in v1), Q3 yes (sweep out-of-band orders on mark moves).** The spec above already
follows these answers; the questions are kept below for the record.

**Q1. Should a slot in margin call be cured by a top-up from the free balance when its
owner places an order that adds exposure?**
- *Recommended: yes* (what section 5.2 specifies). If the free balance covers `IM(W')`, top
  up and accept; the slot leaves margin call. Reject with `MarginCall` only when it can't.
- *Why it matters:* top-ups are "just enough" and fees come out of `locked`, so a position
  opened by a taker with no other orders starts below IM by its own fee, plus however far
  from the mark it filled. In T3, A enters at `E = 3,959,600` against `IM = 5,000,000`.
  Under the literal rule ("in margin call, only strictly reducing orders"), such a trader
  could not add to the position or quote on the increasing side, even with ample free
  balance, until they reduce. In the M3 load, taker accounts would hit this after most
  opening fills. (Market makers quoting both sides mostly stay healthy: their collateral
  covers `IM(W)`, which is at least `IM(abs(pos))`.)
- *Safety is unchanged:* the order is still margin-checked to `IM(W')`, so claim 1 of 5.3
  holds: with the top-up paid from its own free balance, a band-edge flip can't take the
  slot below zero. The collusion test T5 still gets `MarginCall`, because A has no free
  balance to top up with.
- *Alternative:* the literal rule, a one-line change (5.2), which matches Polymarket's
  "Reduce-only" wording but not its "or deposit collateral" option, since v1 has no
  client add-margin command.

**Q2. Should the insurance fund be able to place orders, to unwind the positions it
absorbs?**
- *Recommended: no in v1.* Its orders are rejected (`ReservedAccount`); its positions stay
  until opposite absorbs net them. An account that is exempt from margin checks and can
  still trade is a hole, and unwinding belongs to the "Later" waterfall (INFO.md 12.2).
- *Cost:* the fund's exposure only changes through absorbs, and a market where the fund
  holds a position can never be reconfigured with `SetMarketParams` (its tier table still
  can, 6.9).

**Q3. When the mark moves, should resting orders that are now outside the price band be
cancelled?**
- *Recommended: yes* (what section 5.4 specifies). After each `SetMark`, bids above the new
  upper edge and asks below the new lower edge are cancelled with reason `PriceBand`, and
  the collateral behind them is released.
- *Why it matters:* the band protects the fund only if orders fill near the current mark.
  An order placed inside the band of an old mark can be far through the current one. Two
  reviewers each built a sequence of ordinary client commands that makes the fund pay at a
  mark reached by small steps, with no gap: in F1 a bid left at 100,000 while the mark
  drifts to 80,000 costs the fund 15,000,000 when a colluder sells into it; in H1 a stale
  closing order plus a release costs it 7,000,002. With the sweep both cost nothing (T6).
- *Cost:* a client's order can disappear on a mark move, which clients and the M3
  synthetic makers must handle like any unrequested cancel. A `SetMark` with nothing out
  of band pays two comparisons.
- *Alternatives:* (a) check lazily, just before an incoming order would match a stale one:
  the same protection, but the cancels then appear inside another account's command, and
  "every resting order is inside the band" no longer holds, so the release rule's safety
  argument gets harder to state. (b) Keep stale orders and accept the hole: not
  recommended, since any two colluding accounts can drain the fund with a slow drift.

## 18. Review log

Three reviews of the first draft (numerics: H1, M1, L1 to L5; exploits: F1 to F4;
engineering: R1 to R13). Every finding was checked on the integer model before the spec was
changed.

| ID | Verdict | What changed |
|---|---|---|
| H1 | Accepted | Fixed by the `SetMark` sweep (5.4), not by the proposed stale-price add-on: the add-on doesn't stop F1 (a flat slot is never touched between the drift and the fill) and needs per-slot running price bounds. Claims in 5.3, 10.1 and D-015 rewritten as claims 1 to 3; the sequence is a further unit test (fund unchanged; −7,000,002 without the sweep, confirmed). |
| M1 | Accepted | Band rule 2 added to `SetMarketParams` (5.3, 6.8) with the floors table; the model reproduced both counterexamples (−1,000 at mark 100 on 20x and at 250 on 50x) and none at the floors. Test markets now use `min_price` 1,000. |
| L1 | Accepted | Covered by rule 2: `Lmax` 1 with band = fee = 225,000 is now rejected (S < 0). |
| L2 | Accepted | 2.4 now proves `abs(cost) <= abs(pos) × max_price` for both signs (added to I8) and gives `locked < 2^55` at command boundaries, equity below `2^56`. |
| L3 | Accepted | "May be <= 0" replaced by debug assertions `key_long < mark < key_short` at every re-key (9.1); the naive search is bounded (R8). |
| L4 | Accepted | Every `SetMarketParams` check is computed in `i128` (6.8); widths table in 2.4. |
| L5 | Accepted | Three boundary IM cases added to the tier test (5,000,000 / 4,999,999 lots; chosen 20 below the 25x tier); 6.8's fee-sum reasoning now uses `ceil(a) + ceil(c) >= a + c`. |
| F1 | Accepted | The sweep (5.4, C31, I16, `cancel_beyond`, cancel reason `PriceBand`) and test T6. The finding's step 4 overstated A's release (5,000,000); it is 100,000 at mark 98,000 (98,050 at 98,039), because A#2 keeps `W` at 1,000. User-visible, so asked as Q3. |
| F2 | Accepted | `SetRiskTier` gains `count`; rows are staged and committed in one step, also on a live market (tiers change only IM). `SetMarketParams` clears the table and `SetMark` needs a committed one (`NoRiskTiers`), which closes the new-market window without a separate "committed" flag on orders. |
| F3 | Accepted as a documented risk | 5.3 claim 3 states the bound and the example (224,015,813, reproduced exactly), C32, D-015; pinned by a unit test. The second index stays "Later" (INFO.md 12.2). |
| F4 | Partly accepted | The freeze and its 1-lot cost are documented (6.8, C15, Q2); the tier table, the main risk lever, can now change live (F2). Rejected for v1: an operator cancel and live fee or band changes (each needs its own rules; v1 configures markets before opening them), and leaving the fund out of "empty" (its position needs the mark and price range that `SetMarketParams` resets). |
| R1 | Accepted | Slots live in their market and are created only by `slot_or_create`, which also appends to `market.accounts` (kept in every mode); `slot_markets` removed, `Withdraw` loops over markets; I20 checks the withdrawal rule from the snapshot. |
| R2 | Accepted | `EngineSnapshot` defined field by field, with what is left out and why (15.5). |
| R3 | Accepted | 14.3 says what equivalence proves; independent checker; command properties I17 to I20; planted-bug list. |
| R4 | Accepted | I2 and I3 restated per event block (12); shadow ledger (14.3). |
| R5 | Accepted | Allocation test design, warm-up, fallback threshold, `LiquidationIndex` type, `touched` capacity. The scratch bound is `n + 2`, not `n + 3`: a place emits at most `Ack`, one event per resting order and an IOC remainder. |
| R6 | Accepted | Cancel is pre-checked with `book.order`; `assert!` on the book's first event; the `Reject` row of 7.1 is now "never". |
| R7 | Accepted | Twin step, tier-bound sizes, coverage table with minimums, all four modes, engine seed and capacity test (14.3). |
| R8 | Accepted | Naive search bounded by the old and new marks (14.2). |
| R9 | Accepted | Counter rule in 3.5: +1 at the start of every accepted command; new slots have `touched_in = 0`. |
| R10 | Accepted | (a) emission conditions in 8.2 and 11; (b) `FUND` has no `Account` entry; (c) `i64::try_from` for `uncovered`; (d) `nonzero_positions` in 10.1; (e) `Reject.account = AccountId::MAX` for market-level commands, rather than reserving account 0. |
| R11 | Accepted | Exact field order in 15.1 (new fields last) and per-struct size asserts; sizes rechecked with the `repr(C)` rules, `RiskTierSet` included. |
| R12 | Accepted | Types of `markets`, `accounts` and slots (3.3, 3.5); capacities in `EngineOptions`; walking by index; a test-only slot setter. |
| R13 | Accepted | `i64`/`i128` table (2.4); band edges stored per mark; `key_dirty`; ablation B also times orders; the O(k) liquidation inside a place noted in 14.1. |
| B1 [build] | Test reworded | Section 13's "removal and shrink in place release collateral in a margin call" can't happen: in margin call `E < IM(abs(pos)) <= IM(W)`, so the release is 0. The unit test checks that both are accepted in margin call and release nothing, and that a shrink releases once the slot is healthy again. |
| B2 [build] | Test reworded | Section 13's "`SetMarketParams` on a market with ... a position (the fund's included)" can't have the fund as the only holder: by I2 someone holds the other side of the fund's position. The unit test checks that the fund's position is counted in `nonzero_positions` and the market is `MarketNotEmpty`, and that it is accepted once a second absorb nets the fund to zero. |
| B3 [build] | Gap filled | 14.3 doesn't give the 50x market's fees. The property test uses SP500's taker fee (400 ppm) with a maker rebate of 50 ppm, as the spec's model runs did, so that negative fees and their rounding are exercised; the unit tests keep SP500's live 400/125. |
| B4 [build] | Gap noted | 14.4 doesn't size the allocation flow, and the index's allocations depend on it: in `engine/tests/no_alloc.rs`'s flow, `Fast` makes none with 8 accounts (each side of the index fits in one B-tree node) and 468 allocations plus 468 frees per 10,000 commands with 256 accounts (94 per 1,000, above step 4's threshold of 1). The test reports both; `NaiveLiquidation` makes none in either. |
| P1 [review] | Accepted: D-018's fallback taken | 14.4 step 4's switch condition held: in `no_alloc.rs`'s flow with 256 accounts, `Fast`'s `BTreeSet` made 468 allocations and 468 frees per 10,000 commands (47 allocations per 1,000 against a threshold of 1; B4). `LiquidationIndex` (9.3) is now an indexed heap per side: a heap in a `Vec`, four children per entry, longs ordered by `(Reverse(key), AccountId)` and shorts by `(key, AccountId)` (so ties still go to the lower account), and a map from account to heap position; both are reserved at `slot_capacity`, and an account keeps its map entry once filed, so the map neither grows after an account's first filing nor fills with removal markers. A re-key that stays on its side changes the key in place (`refile`). `Fast` now makes no allocation in that flow, which `no_alloc.rs` asserts instead of reporting; `engine_regressions.rs` checks a 300-per-side heap's walk order against the scan. Interleaved runs of ablation B and the risk-cost bench on a loaded host (load average 6 to 15, so only the ratios mean much; medians of three, `BTreeSet` against heap): a place took 480 against 456 ns at n = 1,000 and 548 against 353 ns at n = 1,000,000, a cancel 412 against 336 and 450 against 268 ns, and a command of the deep flow 558 against 503 ns. The walk pays for it: taking the first entry out moves the last one down from the top, updating the position map at each level, where a B-tree just drops its first element, so a `SetMark` that liquidates 100 of 1,000,000 positions took 56 against 24 µs (the scan: 140 to 210 ms). 9.3's listing, C29 and D-018 still describe the `BTreeSet`, and BENCHMARKS.md has no M2 entry yet: both are for the owner to update. |
| P2 [review] | Accepted: counts stated, two repeats removed | 14.1's "two hash lookups (account, slot)" reads as an operation count and isn't one: each step looks its entry up by id again. A place that rests with a top-up made 14 lookups of its two entries, and each fill 7 more. `Market::trade` now returns the slot it changed (3 lookups per fill) and a top-up returns its events (12 per such place); `engine.rs`'s Complexity paragraph gives these counts and the cost of a repeat (about 6 to 11 ns for an entry in cache). Handing `&mut Slot` and `&mut Account` from step to step, which would save most of the remaining repeats (roughly 60 to 110 ns per place), was not done: it needs split borrows in most handlers. |
| BR1 [review] | Accepted: code docs corrected | (The build review's R1, renamed here to keep it apart from the first review's R1.) 14.1's "one `i128` fee product per fill" is two, maker and taker; `engine.rs` says so. `state.rs`'s Contract said the engine makes every change that moves money, but `Market::trade`, `empty_slot` and `net_into_fund` move money inside a market; it now says which. |
| E1 [review] | Evidence for D-017 | 14.3's planted bugs, one at a time on the final code, each file restored byte for byte (checked with sha256). Each fails the debug property test in its first scenarios, within 0.06 to 0.73 s of test time (shrinking off): a long key one tick high, the re-key's debug check of 9.1; ties to the higher account, `Fast`'s events differ from the reference's on a `SetMark`; no subtraction on a `SelfTrade` cancel, I4; release computed as `locked − IM(W)`, the independent checker's I18; a top-up one micro too large, the checker's I17; the sweep skipped, I16. MM rounded down, a formula every mode shares: four unit tests of `money.rs` (`requirements_round_up` and three key tests), and in the property test the re-key's debug check (I10 in a release build), because the closed-form key no longer matches the direct check. |
