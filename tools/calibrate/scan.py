"""One read-only pass over the recorded Polymarket Perps data: what the profile needs from it.

Reads `data/rest/instruments/2026-09-30.json` and the hourly websocket files
`data/ws/<day>/<hh>.jsonl.gz` (one JSON object per line; `book::<iid>` and `tickers::<iid>`
frames are sampled by the recorder to the last frame of each server second, `trades::<iid>`
frames are all kept). Never writes anything.

Each hour file is scanned by one worker process (`scan_hour`), and `scan` merges the results:
- **Start prices**: the last `tickers::<iid>` frame with server time <= T0, field `mark`.
- **Maker weights**: level changes between consecutive 1-s book snapshots of a market, counted
  only at prices both snapshots show ("inner"), over the clean window.
- **Taker weights**: trade prints (deduplicated by trade id) in the clean window, grouped into
  taker events by (market, side, exact trade time in ms).
- **Marks per second** over the flow sample, for the persistent-jump count.
- **Book shape** over the shape hours: spread, gaps between levels (on sides showing all 20
  levels), size of each level, level by level.

Standard library only (`tools/calibrate/README.md`).
"""

import gzip
import json
import math
import os
from array import array
from collections import Counter, defaultdict
from multiprocessing import Pool

# ---------------------------------------------------------------------------------------------
# The sample (README.md, "Sample").

# 2026-09-30 10:00:00.000 UTC, in ms: the start prices are the marks at this instant.
T0_MS = 1_790_762_400_000
# The clean window of the per-market weights: from the first frame of the multi-connection
# recorder (2026-09-29 15:04:54.611 UTC; before it, one connection recorded books for only 50
# markets) to the end of the last hour file complete when the calibration ran (2026-09-30
# 15:00:00 UTC). 22.08 effective hours: 2026-09-29 17:10 to 19:00 is a recording gap.
CLEAN_START_MS = 1_790_694_294_611
CLEAN_END_MS = 1_790_780_400_000
# Consecutive book snapshots further apart than this are not compared (a recording gap).
MAX_SNAPSHOT_GAP_MS = 5_000

# Hour files of the weights and the price dynamics: 2026-09-29 14:00 to 2026-09-30 14:59 UTC.
FLOW_HOURS = [("2026-09-29", h) for h in range(14, 24)] + [("2026-09-30", h) for h in range(0, 15)]
# The feed shows at most this many levels a side.
LEVELS = 20

# Hour files of the book shape: the independent check's hold-out hours (README.md), an evening
# hour after the US close, a night hour and a US-session hour; the corrected values come from
# these three.
SHAPE_HOURS = [("2026-09-29", 22), ("2026-09-30", 3), ("2026-09-30", 15)]

INSTRUMENTS_FILE = "rest/instruments/2026-09-30.json"

# ---------------------------------------------------------------------------------------------
# Market classes (D-034).

MAJORS = {"BTC-USD", "ETH-USD", "SOL-USD"}
MACRO = {"SP500-USD", "NAS100-USD", "GOLD-USD", "SILVER-USD", "WTIOIL-USD", "BRENTOIL-USD"}
BOOK_CLASSES = ["majors", "alt_crypto", "longtail_crypto", "tradfi_equities", "tradfi_macro"]
LEVERAGE_CLASSES = ["L50", "L20", "L10", "L3-5"]


def book_class(instrument):
    """Book-shape class: majors; alt crypto (10x or more); long-tail crypto (5x or less);
    tradfi macro (two indexes and four commodities); tradfi equities (the rest)."""
    symbol = instrument["symbol"]
    if symbol in MAJORS:
        return "majors"
    if instrument["category"] == "crypto":
        return "alt_crypto" if instrument["max_leverage"] >= 10 else "longtail_crypto"
    return "tradfi_macro" if symbol in MACRO else "tradfi_equities"


def leverage_class(instrument):
    """Price-dynamics class, by maximum leverage: 50x, 20x, 10x, or 3x and 5x."""
    lev = instrument["max_leverage"]
    return "L50" if lev >= 50 else "L20" if lev >= 20 else "L10" if lev >= 10 else "L3-5"


def load_instruments(data_dir):
    """The 88 instruments, by id, as published."""
    with open(os.path.join(data_dir, INSTRUMENTS_FILE)) as f:
        return {x["instrument_id"]: x for x in json.load(f)}


# ---------------------------------------------------------------------------------------------
# Units.


def to_units(text, decimals):
    """A decimal string as an integer count of 10^-decimals: a price in our ticks, or a size in
    our lots (pd + qd = 6, so ticks x lots = micro-dollars, D-004). Exact: no floats."""
    whole, _, fraction = text.partition(".")
    fraction = fraction.rstrip("0")
    assert len(fraction) <= decimals and "e" not in text.lower(), text
    return int(whole or "0") * 10**decimals + int(fraction.ljust(decimals, "0") or "0")


def real_tick(price_ticks):
    """Polymarket's price grid at a price: 5 significant figures, so
    `10^max(0, digits(price_ticks) - 5)` of our ticks."""
    return 10 ** max(0, len(str(price_ticks)) - 5)


def round_bps(x):
    """The spread histogram's key: 0.01 bps below 100 bps, whole bps above (as the check)."""
    return round(x, 2) if x < 100 else float(round(x))


# ---------------------------------------------------------------------------------------------
# Maker activity: inner level changes between two snapshots.


def inner_changes(prev, cur, is_ask):
    """Levels that appeared, disappeared or changed size between two snapshots of one side,
    counted only at prices inside the 20th level of both (or anywhere on a side showing fewer
    than 20 levels), so that levels merely scrolling in or out of view don't count.
    `prev` and `cur` are lists of [price, size] strings, best first."""
    before = dict((p, s) for p, s in prev)
    after = dict((p, s) for p, s in cur)
    far = math.inf if is_ask else -math.inf
    cap_prev = float(prev[-1][0]) if len(prev) >= 20 else far
    cap_cur = float(cur[-1][0]) if len(cur) >= 20 else far
    if is_ask:
        cap = min(cap_prev, cap_cur)
        visible = lambda price: float(price) <= cap  # noqa: E731
    else:
        cap = max(cap_prev, cap_cur)
        visible = lambda price: float(price) >= cap  # noqa: E731
    changes = 0
    for price, size in after.items():
        if before.get(price) != size and visible(price):
            changes += 1
    for price in before:
        if price not in after and visible(price):
            changes += 1
    return changes


def book_changes(prev, cur):
    """Inner level changes of both sides between two snapshots (dicts with "a" and "b")."""
    return inner_changes(prev["a"], cur["a"], True) + inner_changes(prev["b"], cur["b"], False)


# ---------------------------------------------------------------------------------------------
# Book shape of one snapshot.


class Shape:
    """Book-shape histograms of the shape hours, per market, per class and per level."""

    def __init__(self):
        self.spread_bps = defaultdict(Counter)  # market -> round_bps(spread) -> snapshots
        self.spread_ticks = defaultdict(Counter)  # market -> spread in real ticks -> snapshots
        self.gaps = defaultdict(Counter)  # (class, bucket) -> gap in real ticks -> count
        self.levels = Counter()  # class -> levels
        self.levels_at = Counter()  # (class, level k, 0-based) -> levels
        self.nondust_at = defaultdict(Counter)  # (class, k) -> floor(200 log10 USD) -> non-dust levels
        self.market_levels = Counter()  # market -> levels
        self.market_dust = Counter()  # market -> dust levels

    def add(self, iid, cls, asks, bids):
        """One snapshot: `asks` and `bids` as (price ticks, size lots), best first."""
        if asks and bids:
            best_ask, best_bid = asks[0][0], bids[0][0]
            spread = best_ask - best_bid
            mid = (best_ask + best_bid) / 2.0
            self.spread_bps[iid][round_bps(spread / mid * 1e4)] += 1
            self.spread_ticks[iid][spread // real_tick(best_bid)] += 1
        for side in (asks, bids):
            for k, (price, size) in enumerate(side):
                usd = price * size / 1e6
                if usd <= 0:
                    continue
                self.levels[cls] += 1
                self.levels_at[(cls, k)] += 1
                self.market_levels[iid] += 1
                if 10.0 <= usd <= 12.5:
                    self.market_dust[iid] += 1
                else:
                    self.nondust_at[(cls, k)][math.floor(math.log10(usd) * 200)] += 1
            # Gaps only on a side that shows all 20 levels: a generator's ladder always does,
            # and the thinner sides' far gaps (stale orders, hundreds of bps out) would put its
            # deep levels far beyond where a 20-level book has them.
            if len(side) == LEVELS:
                for k in range(len(side) - 1):
                    (p0, _), (p1, _) = side[k], side[k + 1]
                    ticks = abs(p1 - p0) // real_tick(min(p0, p1))
                    self.gaps[(cls, gap_bucket(k))][ticks] += 1

    def merge(self, other):
        for name, value in vars(other).items():
            mine = getattr(self, name)
            if isinstance(value, Counter):
                mine.update(value)
            else:
                for key, counter in value.items():
                    mine[key].update(counter)


def gap_bucket(k):
    """The gap after level `k + 1` (0-based `k`): gaps 1-4, 5-9 and 10-19."""
    return "1-4" if k < 4 else "5-9" if k < 9 else "10-19"


# ---------------------------------------------------------------------------------------------
# One hour file.


def scan_hour(job):
    """Scans one hour file. `job` = (path, instruments, in_flow_sample, in_shape_sample)."""
    path, instruments, flow, shape_hour = job
    classes = {iid: book_class(x) for iid, x in instruments.items()}
    result = {
        "path": path,
        "trades": [],  # (tid, iid, side, price, qty, ts_ms, settlement), every print
        "first_book": {},  # iid -> (ts, data): first snapshot in the clean window
        "last_book": {},  # iid -> (ts, data): last snapshot in the clean window
        "inner": Counter(),  # iid -> inner level changes between snapshots of this file
        "pairs": Counter(),  # iid -> snapshot pairs compared
        "t0_ticker": {},  # iid -> (ts, mark): last ticker at or before T0
        "marks": defaultdict(dict),  # iid -> second -> mark (float), the flow sample only
        "seconds": set(),  # seconds with a book snapshot of any market, the flow sample only
        "shape": Shape() if shape_hour else None,
    }
    with gzip.open(path, "rt") as f:
        for line in f:
            if not line.startswith('{"recv_ms":') or '"frame"' not in line[:40]:
                continue  # markers (file opened, connection events)
            frame = json.loads(line)["frame"]
            if "ch" not in frame:
                continue  # a reply to a subscription, {"id": n, "data": [{"status": "ok"}]}
            kind, _, iid_text = frame["ch"].partition("::")
            if kind == "trades":
                if flow:
                    for t in frame["data"]:
                        settled = bool(t.get("settlement"))
                        trade = (t["tid"], t["iid"], t["side"], t["p"], t["qty"], t["ts"], settled)
                        result["trades"].append(trade)
                continue
            iid, ts = int(iid_text), frame["ts"]
            if iid not in instruments:
                continue
            if kind == "tickers":
                mark = frame["data"].get("mark")
                if mark is None:
                    continue
                if ts <= T0_MS and ts >= result["t0_ticker"].get(iid, (-1, None))[0]:
                    result["t0_ticker"][iid] = (ts, mark)
                if flow:
                    result["marks"][iid][ts // 1000] = float(mark)
            elif kind == "book":
                data = frame["data"]
                if flow:
                    result["seconds"].add(ts // 1000)
                if flow and CLEAN_START_MS <= ts < CLEAN_END_MS:
                    add_book_pair(result, iid, ts, data)
                if shape_hour:
                    pd = instruments[iid]["price_decimals"]
                    qd = instruments[iid]["quantity_decimals"]
                    asks = [(to_units(p, pd), to_units(q, qd)) for p, q in data.get("a", [])]
                    bids = [(to_units(p, pd), to_units(q, qd)) for p, q in data.get("b", [])]
                    result["shape"].add(iid, classes[iid], asks, bids)
    # Plain dicts and arrays pickle faster than nested defaultdicts of floats.
    result["marks"] = {
        iid: (array("q", sorted(m)), array("d", (m[s] for s in sorted(m))))
        for iid, m in result["marks"].items()
    }
    return result


def add_book_pair(result, iid, ts, data):
    """Compares a clean-window snapshot with the market's previous one in this file."""
    last = result["last_book"].get(iid)
    if last is None:
        result["first_book"][iid] = (ts, data)
    elif ts <= last[0]:
        return  # a duplicate or out-of-order frame
    elif ts - last[0] <= MAX_SNAPSHOT_GAP_MS:
        result["inner"][iid] += book_changes(last[1], data)
        result["pairs"][iid] += 1
    result["last_book"][iid] = (ts, data)


# ---------------------------------------------------------------------------------------------
# The whole pass.


def hour_path(data_dir, day, hour):
    return os.path.join(data_dir, "ws", day, "%02d.jsonl.gz" % hour)


def scan(data_dir, processes):
    """Scans every hour file of the sample, `processes` at a time, and merges the results:
    a dict with `instruments`, `t0_ticker`, `inner`, `pairs`, `trades`, `marks`, `seconds` and
    `shape`."""
    instruments = load_instruments(data_dir)
    hours = sorted(set(FLOW_HOURS) | set(SHAPE_HOURS))
    jobs = [
        (hour_path(data_dir, day, h), instruments, (day, h) in FLOW_HOURS, (day, h) in SHAPE_HOURS)
        for day, h in hours
    ]
    for path, *_ in jobs:
        if not os.path.exists(path):
            raise SystemExit("missing hour file %s (the sample needs every hour of README.md)" % path)
    with Pool(processes) as pool:
        results = pool.map(scan_hour, jobs, chunksize=1)  # in hour order

    merged = {
        "instruments": instruments,
        "t0_ticker": {},
        "inner": Counter(),
        "pairs": Counter(),
        "trades": {},
        "marks": defaultdict(dict),
        "seconds": set(),
        "shape": Shape(),
    }
    flow_results = [r for r, job in zip(results, jobs) if job[2]]
    for r in results:
        for iid, (ts, mark) in r["t0_ticker"].items():
            if ts >= merged["t0_ticker"].get(iid, (-1, None))[0]:
                merged["t0_ticker"][iid] = (ts, mark)
        if r["shape"] is not None:
            merged["shape"].merge(r["shape"])
    for r in flow_results:
        merged["inner"].update(r["inner"])
        merged["pairs"].update(r["pairs"])
        merged["seconds"].update(r["seconds"])
        for t in r["trades"]:
            if merged["trades"].setdefault(t[0], t) != t:
                raise SystemExit("trade id %d seen twice with different content" % t[0])
        for iid, (seconds, marks) in r["marks"].items():
            merged["marks"][iid].update(zip(seconds, marks))
    # Snapshot pairs across two consecutive hour files: the last snapshot of one with the first
    # of the next, if they are close enough.
    for a, b in zip(flow_results, flow_results[1:]):
        for iid in instruments:
            last, first = a["last_book"].get(iid), b["first_book"].get(iid)
            if last and first and 0 < first[0] - last[0] <= MAX_SNAPSHOT_GAP_MS:
                merged["inner"][iid] += book_changes(last[1], first[1])
                merged["pairs"][iid] += 1
    return merged


def default_processes():
    """Worker processes: at most 6, and never the last CPU, which the recorders use (the `dev`
    script keeps builds off it too)."""
    cpus = os.cpu_count() or 1
    if hasattr(os, "sched_setaffinity") and cpus >= 3:
        os.sched_setaffinity(0, range(cpus - 1))  # inherited by the workers
    return max(1, min(6, cpus - 2))
