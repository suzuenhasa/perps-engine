"""From the scan (`scan.py`) and the copied calibration outputs (`inputs/`) to the profile.

The profile is a plain dict, written as `profile-2026-09-30.json` and rendered into Rust by
`rust.py`. It holds derived parameters only, never recorded data. Its sections follow D-034:
the 88 markets; book shape per class; price moves per max-leverage class; jumps; maker
activity; takers; bursts; shocks.

**Everything a flow draws content from is an integer** (loadgen's rule: no floats decide what a
flow contains). So each continuous distribution is given twice: its fitted parameters, for the
reader, and a table of equally likely values computed here from them, which is what a
generator samples (`quantile_table`). Units are in the key names: `_ppm` parts per
million, `_milli` thousandths, `_centibps` hundredths of a basis point, `_micros` micro-dollars
(D-004), `_ticks` our price ticks.

Where a value comes from `inputs/` rather than from the data, the code says so; README.md lists
the sources and the independent check's corrections.
"""

import math
import statistics
from collections import Counter, defaultdict
from datetime import datetime

import scan

NORMAL = statistics.NormalDist()
# z at p = 0.9: split lognormals are fitted to the 10th, 50th and 90th percentiles.
Z90 = NORMAL.inv_cdf(0.9)

# Clip menus (D-034): the fixed-notional sizes that stack in the books, in USD. A level's size
# counts as a clip when it lies within +-3% of one (the independent check's window).
CLIP_MENUS = {
    "majors": [],
    "alt_crypto": [6_250],
    "longtail_crypto": [6_250],
    "tradfi_equities": [25_000, 50_000, 100_000],
    "tradfi_macro": [25_000, 50_000, 100_000],
}
CLIP_WINDOW = 0.03

# Dust: orders just above the $10 minimum notional. The books count a level of $10 to $12.50
# as dust; a generator draws its size from [$10.50, $11.60), where 94% of the takers' dust
# lies (flow.json), for makers and takers alike.
DUST_DRAW_CENTS = (1_050, 1_160)

# Sizes of the equally likely tables.
SMALL_TABLE = 64  # spreads, gap tails, depth backgrounds
FINE_TABLE = 1_024  # |z| of a normal draw; the takers' continuous notional
JUMP_TABLE = 16  # sizes of persistent jumps (276 observed)
CLUSTER_GAP_TABLE = 16  # gaps inside a taker cluster

# A persistent jump (flow.json's definition): a 1-s mark move over 50 bps whose move from the
# second before to 5 s after is still over 25 bps (half the threshold).
JUMP_BPS = 50
# Taker events closer than this, exchange-wide, form one cluster (flow.json).
CLUSTER_GAP_MS = 50

# Full hours of the flow sample, all 88 markets recorded (flow.json): the per-hour jump rate's
# median is taken over these.
FULL_HOURS = [("2026-09-29", h) for h in (15, 16, 19, 20, 21, 22, 23)] + [
    ("2026-09-30", h) for h in range(0, 15)
]


# ---------------------------------------------------------------------------------------------
# Small helpers.


def ppm(x):
    """A share as an integer count of millionths."""
    return round(x * 1_000_000)


def milli(x):
    return round(x * 1_000)


def ppm_split(weights):
    """Integer shares in millionths of `weights` (counts or floats), summing to exactly
    1,000,000: each takes the floor of its exact share, and the millionths left over go to the
    largest remainders (ties to the earlier entry)."""
    total = sum(weights)
    exact = [w * 1_000_000 / total for w in weights]
    shares = [math.floor(x) for x in exact]
    order = sorted(range(len(exact)), key=lambda i: (-(exact[i] - shares[i]), i))
    for i in order[: 1_000_000 - sum(shares)]:
        shares[i] += 1
    return shares


def counter_quantile(counter, p):
    """The smallest key whose cumulative count reaches `p` of the total (the check's rule)."""
    total, running = sum(counter.values()), 0
    for key in sorted(counter):
        running += counter[key]
        if running >= p * total:
            return key
    raise ValueError("empty counter")


def interpolated_quantile(values, p):
    """Linear interpolation between order statistics, as the calibration scripts did."""
    values = sorted(values)
    k = (len(values) - 1) * p
    low = math.floor(k)
    high = min(low + 1, len(values) - 1)
    return values[low] + (values[high] - values[low]) * (k - low)


def quantile_table(inverse_cdf, n):
    """`n` equally likely values of a distribution: its quantiles at the middle of each of `n`
    equal slices of probability, `inverse_cdf((i + 0.5) / n)`. A draw picks one uniformly."""
    return [inverse_cdf((i + 0.5) / n) for i in range(n)]


def utc(day, hour):
    return int(datetime.fromisoformat("%sT%02d:00:00+00:00" % (day, hour)).timestamp())


# ---------------------------------------------------------------------------------------------
# The profile.


def derive(scanned, flow, book):
    """The profile, from the scan and the copied `flow.json` and `book.json`."""
    shape = scanned["shape"]
    moves = move_models(flow)
    spreads, market_spread = spread_models(scanned["instruments"], shape)
    events = taker_events(scanned)
    return {
        "constants": {
            "levels_per_side": scan.LEVELS,
            "mark_every_ms": 200,
            "min_notional_micros": 10 * 1_000_000,
            "dust_draw_cents": list(DUST_DRAW_CENTS),
        },
        "markets": markets(scanned, flow, events, moves, market_spread),
        "book_shapes": [book_shape(c, shape, c if c in spreads else "one_tick") for c in scan.BOOK_CLASSES],
        # The split-lognormal spreads: one per class (pooled), then one per macro market.
        "spreads": [spreads[c] for c in scan.BOOK_CLASSES if c in spreads]
        + [spreads[k] for k in sorted(spreads) if k not in scan.BOOK_CLASSES],
        "moves": [moves[c] for c in scan.LEVERAGE_CLASSES],
        # |z| of a standard normal: the quantile at p of |z| is the normal's at 0.5 + p / 2.
        "half_normal_milli": [
            milli(z) for z in quantile_table(lambda p: NORMAL.inv_cdf(0.5 + p / 2), FINE_TABLE)
        ],
        "jumps": jumps(scanned),
        "maker_activity": maker_activity(book),
        "takers": takers(scanned, events),
        "taker_notional": taker_notional(flow),
        "bursts": bursts(flow),
        "shocks": shocks(flow),
    }


def taker_events(scanned):
    """Taker events over the flow sample: the prints of one market, side and millisecond, as
    (ms, market, side), in time order. Settlement prints are not taker orders (there are none)."""
    return sorted({(t[5], t[1], t[2]) for t in scanned["trades"].values() if not t[6]})


def in_clean_window(ms):
    return scan.CLEAN_START_MS <= ms < scan.CLEAN_END_MS


# ---------------------------------------------------------------------------------------------
# The markets.


def markets(scanned, flow, events, moves, market_spread):
    """One row per instrument, by id: the published parameters, the start price and its grid,
    the activity weights over the clean window (maker: inner level changes; taker: taker
    events), and the book and move parameters that differ per market."""
    instruments = scanned["instruments"]
    iids = sorted(instruments)
    maker = ppm_split([scanned["inner"][i] for i in iids])
    clean = Counter(iid for ms, iid, _ in events if in_clean_window(ms))
    taker = ppm_split([clean[i] for i in iids])
    shape = scanned["shape"]
    per_market_moves = flow["price_dynamics_1s"]["per_market_mark"]
    rows = []
    for n, iid in enumerate(iids):
        x = instruments[iid]
        pd, qd = x["price_decimals"], x["quantity_decimals"]
        assert pd + qd == 6, x["symbol"]
        _, mark = scanned["t0_ticker"][iid]
        start = scan.to_units(mark, pd)
        move_class = scan.leverage_class(x)
        # The class mixture has unit mean square. Scaled by this RMS, a nonzero move gives the
        # market its own 1-s standard deviation, zeros included: sigma^2 = (1 - no move) RMS^2.
        sigma = per_market_moves[x["symbol"]]["sd_bps"]
        rms = sigma / math.sqrt(1 - moves[move_class]["no_move_ppm"] / 1e6)
        rows.append(
            {
                "id": iid,
                "symbol": x["symbol"],
                "book_class": scan.book_class(x),
                "leverage_class": move_class,
                "price_decimals": pd,
                "qty_decimals": qd,
                "max_leverage": x["max_leverage"],
                "tiers": [[scan.to_units(t["lower_bound"], 6), t["max_leverage"]] for t in x["risk_tiers"]],
                "start_price_ticks": start,
                "price_grid_ticks": scan.real_tick(start),
                "maker_weight_ppm": maker[n],
                "taker_weight_ppm": taker[n],
                "spread": market_spread[iid],
                "dust_ppm": ppm(shape.market_dust[iid] / shape.market_levels[iid]),
                "move_rms_millibps": milli(rms),
                "max_notional_micros": scan.to_units(x["max_market_notional"], 6),
            }
        )
    return rows


# ---------------------------------------------------------------------------------------------
# Book shape (shape hours).


def split_lognormal(counter):
    """A split lognormal fitted to a spread histogram in bps: the median, and below and above
    it the log-sd that puts the 10th and the 90th percentiles where they are."""
    p10, p50, p90 = (counter_quantile(counter, p) for p in (0.1, 0.5, 0.9))
    below, above = math.log(p50 / p10) / Z90, math.log(p90 / p50) / Z90

    def inverse_cdf(p):
        z = NORMAL.inv_cdf(p)
        return p50 * math.exp(z * (below if z < 0 else above))

    return {
        "median_centibps": round(p50 * 100),
        "sigma_below_milli": milli(below),
        "sigma_above_milli": milli(above),
        "centibps": [round(x * 100) for x in quantile_table(inverse_cdf, SMALL_TABLE)],
    }


def spread_models(instruments, shape):
    """Spread models: per class, a split lognormal pooled over its markets that are not
    tick-bound; one real tick for a tick-bound market (median spread of one real tick: the
    majors, SP500, NAS100 and GOLD); and for each other macro market its own split lognormal.
    Returns the models by name, and each market's model name ("one_tick", a class, or a symbol)."""
    tick_bound = {iid for iid in instruments if counter_quantile(shape.spread_ticks[iid], 0.5) == 1}
    pooled = defaultdict(Counter)
    models, market_spread = {}, {}
    for iid in sorted(instruments):
        cls, symbol = scan.book_class(instruments[iid]), instruments[iid]["symbol"]
        if iid in tick_bound:
            market_spread[iid] = "one_tick"
        elif cls == "tradfi_macro":
            models[symbol] = dict(name=symbol, **split_lognormal(shape.spread_bps[iid]))
            market_spread[iid] = symbol
        else:
            pooled[cls].update(shape.spread_bps[iid])
            market_spread[iid] = cls
    for cls, counter in pooled.items():
        models[cls] = dict(name=cls, **split_lognormal(counter))
    return models, market_spread


def gap_model(counter):
    """Gaps between consecutive levels of one side, in real ticks: the share of each gap of 1
    to 10 ticks, and beyond that `10 + exp(N(mu, sd))` ticks, fitted to ln(gap - 10)."""
    tail = {ticks: n for ticks, n in counter.items() if ticks > 10}
    shares = ppm_split([counter.get(t, 0) for t in range(1, 11)] + [sum(tail.values())])
    count = sum(tail.values())
    mu = sum(math.log(t - 10) * n for t, n in tail.items()) / count
    sd = math.sqrt(sum((math.log(t - 10) - mu) ** 2 * n for t, n in tail.items()) / count)
    table = quantile_table(lambda p: 10 + math.exp(mu + sd * NORMAL.inv_cdf(p)), SMALL_TABLE)
    return {
        "one_to_ten_ppm": shares[:10],
        "beyond_ten_ppm": shares[10],
        "tail_mu_milli": milli(mu),
        "tail_sd_milli": milli(sd),
        "tail_ticks": [max(11, round(x)) for x in table],
    }


def window_bins(usd):
    """The depth histogram's bins (floor(200 log10 USD)) within +-3% of a clip size."""
    low, high = usd / (1 + CLIP_WINDOW), usd * (1 + CLIP_WINDOW)
    return range(math.floor(math.log10(low) * 200), math.floor(math.log10(high) * 200))


def usd_table(counter):
    """The sizes of a depth histogram as 64 equally likely whole dollars: its own quantiles at
    the middle of 64 equal slices, each a bin's centre (a bin covers [10^(b/200),
    10^((b+1)/200)) USD, so its centre in ln USD is (b + 0.5) ln10 / 200). No order rests
    below the $10 minimum: smaller sizes are raised to it."""
    middles = [(i + 0.5) / SMALL_TABLE for i in range(SMALL_TABLE)]
    ln_usd = [(counter_quantile(counter, p) + 0.5) * math.log(10) / 200 for p in middles]
    return [max(10, round(math.exp(x))) for x in ln_usd]


def book_shape(cls, shape, spread):
    """One class's book: its spread model's name ("one_tick" for the majors), the gaps per
    level bucket (from the sides showing all 20 levels, scan.py), and the sizes of each level,
    1 to 20: dust ($10 to $12.50), one of the clips (+-3%), or else a draw of that level's
    background, the other sizes recorded at that level. Shares are of the level's recorded
    sizes. Level by level, since depth is not flat: level 1 is thin (a median of $1k to $3k),
    and the clips sit at levels 3 to 10 (book.md, section 4). The backgrounds are the
    recorded sizes' own quantiles: they are bimodal (small orders and large ones), which no
    lognormal fits."""
    levels, dust = [], 0
    for k in range(scan.LEVELS):
        total, nondust = shape.levels_at[(cls, k)], shape.nondust_at[(cls, k)]
        dust += total - sum(nondust.values())
        clips, in_windows = [], set()
        for usd in CLIP_MENUS[cls]:
            bins = window_bins(usd)
            in_windows.update(bins)
            clips.append(ppm(sum(nondust[b] for b in bins) / total))
        background = Counter({b: n for b, n in nondust.items() if b not in in_windows})
        levels.append(
            {
                "dust_ppm": ppm((total - sum(nondust.values())) / total),
                "clips_ppm": clips,
                "background_usd": usd_table(background),
            }
        )
    return {
        "class": cls,
        "spread": spread,
        "gaps": [gap_model(shape.gaps[(cls, bucket)]) for bucket in ("1-4", "5-9", "10-19")],
        "dust_ppm": ppm(dust / shape.levels[cls]),
        "clips_usd": CLIP_MENUS[cls],
        "clip_jitter_ppm": ppm(CLIP_WINDOW),
        "levels": levels,
    }


# ---------------------------------------------------------------------------------------------
# Price moves, from flow.json (the 3-component fits are not re-derived here).


def move_models(flow):
    """Per max-leverage class, the 1-s moves of the operator's mark: the share of seconds
    without a move, and a normal scale mixture for the others, in units of the market's RMS
    move (flow.json, `price_dynamics_1s.classes.<class>.mark`)."""
    models = {}
    for cls in scan.LEVERAGE_CLASSES:
        mark = flow["price_dynamics_1s"]["classes"][cls]["mark"]
        mixture = mark["nonzero_shape"]["normal_scale_mixture3"]
        models[cls] = {
            "class": cls,
            "no_move_ppm": ppm(mark["share_zero_change"]),
            "weights_ppm": ppm_split(mixture["w"]),
            "sd_milli": [milli(sd) for sd in mixture["sd"]],
        }
    return models


def jumps(scanned):
    """Persistent jumps of the mark over 50 bps: their rate, the median over the full hours of
    the jumps per market-hour (the mean, 0.14, is set by one episode: 111 of the 276 jumps fell
    in 2026-09-29 23:00-23:59, 92 of them LIT-USD's), and their sizes, over the whole flow
    sample."""
    per_hour = Counter()
    market_seconds = Counter()
    sizes = []
    full = {utc(day, h) // 3600 for day, h in FULL_HOURS}
    for iid, marks in scanned["marks"].items():
        for second in sorted(marks):
            before = marks.get(second - 1)
            if before is None:
                continue  # returns only between consecutive seconds
            hour = second // 3600
            market_seconds[hour] += 1
            move = math.log(marks[second] / before) * 1e4
            later = marks.get(second + 5)
            if abs(move) > JUMP_BPS and later and abs(math.log(later / before) * 1e4) > JUMP_BPS / 2:
                per_hour[hour] += 1
                sizes.append(abs(move))
    rates = [per_hour[h] / (market_seconds[h] / 3600) for h in sorted(full)]
    rate = statistics.median(rates)
    return {
        "over_bps": JUMP_BPS,
        "one_in_market_seconds": round(3600 / rate),
        "per_market_hour_milli": milli(rate),
        "observed": len(sizes),
        "centibps": [
            round(interpolated_quantile(sizes, (i + 0.5) / JUMP_TABLE) * 100) for i in range(JUMP_TABLE)
        ],
    }


# ---------------------------------------------------------------------------------------------
# Maker activity, from book.json.


def maker_activity(book):
    """What a maker's level change is, over the 21 full hours (book.json,
    `overall.requote_1s_lower_bound.all21`): an add, a removal, or a size change up or down;
    and which levels it touches."""
    all21 = book["overall"]["requote_1s_lower_bound"]["all21"]
    mix, buckets = all21["mix"], all21["by_level_bucket_share"]
    kinds = ppm_split([mix["add"], mix["remove"], mix["size_up"], mix["size_down"]])
    levels = ppm_split([buckets["1-5"], buckets["6-10"], buckets["11-20"]])
    return {
        "add_ppm": kinds[0],
        "remove_ppm": kinds[1],
        "size_up_ppm": kinds[2],
        "size_down_ppm": kinds[3],
        "levels_1_5_ppm": levels[0],
        "levels_6_10_ppm": levels[1],
        "levels_11_20_ppm": levels[2],
    }


# ---------------------------------------------------------------------------------------------
# Takers.


def takers(scanned, events):
    """Taker orders: their share of messages over the clean window (against the makers' inner
    level changes, a lower bound: the book is seen once a second), their rate and side over the
    flow sample, and their clusters: events chained while each follows the previous one by at
    most 50 ms, exchange-wide."""
    maker_changes = sum(scanned["inner"].values())
    clean = sum(1 for ms, _, _ in events if in_clean_window(ms))
    clusters, current = [], [events[0]]
    for event in events[1:]:
        if event[0] - current[-1][0] <= CLUSTER_GAP_MS:
            current.append(event)
        else:
            clusters.append(current)
            current = [event]
    clusters.append(current)
    sizes = Counter(len(c) for c in clusters)
    # Within a cluster: the gap to the order before, whether it is in the same market as the
    # order before, and whether it is on the side of the cluster's first order.
    gaps = [c[i][0] - c[i - 1][0] for c in clusters for i in range(1, len(c))]
    same_market = sum(1 for c in clusters for i in range(1, len(c)) if c[i][1] == c[i - 1][1])
    same_side = sum(1 for c in clusters for i in range(1, len(c)) if c[i][2] == c[0][2])
    seconds = len(scanned["seconds"])  # recorded seconds of the flow sample
    return {
        "share_of_messages_ppm": ppm(clean / (maker_changes + clean)),
        "events_per_s_milli": milli(len(events) / seconds),
        "buy_ppm": ppm(sum(1 for e in events if e[2] == "long") / len(events)),
        "cluster_starts_per_s_milli": milli(len(clusters) / seconds),
        "cluster_size_ppm": ppm_split([sizes[n] for n in range(1, max(sizes) + 1)]),
        "cluster_same_market_ppm": ppm(same_market / len(gaps)),
        "cluster_same_side_ppm": ppm(same_side / len(gaps)),
        "cluster_gap_ms": [
            round(interpolated_quantile(gaps, (i + 0.5) / CLUSTER_GAP_TABLE))
            for i in range(CLUSTER_GAP_TABLE)
        ],
    }


def taker_notional(flow):
    """USD per taker order (flow.json, `takers.notional_usd_sampler`; the fit is not
    re-derived): dust, round-number point masses, else a two-lognormal mixture truncated below
    $12.50, given as a table of equally likely values in cents."""
    sampler = flow["takers"]["notional_usd_sampler"]
    fit = sampler["continuous_lognormal_mixture_truncated"]
    components = [
        (fit["w1"], fit["mu1_ln_usd"], fit["sigma1"]),
        (fit["w2"], fit["mu2_ln_usd"], fit["sigma2"]),
    ]
    floor = fit["truncate_below_usd"]

    def cdf(usd):
        return sum(w * NORMAL.cdf((math.log(usd) - mu) / sd) for w, mu, sd in components)

    below = cdf(floor)

    def inverse_cdf(p):
        # Bisection on ln(USD) for the truncated mixture's p-quantile.
        target = below + p * (1 - below)
        low, high = math.log(floor), math.log(1e9)
        for _ in range(100):
            middle = (low + high) / 2
            low, high = (middle, high) if cdf(math.exp(middle)) < target else (low, middle)
        return math.exp((low + high) / 2)

    masses = sampler["point_masses"]
    shares = ppm_split([sampler["share_dust"]] + [m["share"] for m in masses] + [sampler["share_continuous"]])
    return {
        "dust_ppm": shares[0],
        "point_masses": [[round(m["usd"] * 100), s] for m, s in zip(masses, shares[1 : 1 + len(masses)])],
        "continuous_ppm": shares[-1],
        "truncate_below_cents": round(floor * 100),
        "mixture": [
            {"weight_ppm": ppm(w), "mu_milli": milli(mu), "sd_milli": milli(sd)} for w, mu, sd in components
        ],
        "continuous_cents": [round(x * 100) for x in quantile_table(inverse_cdf, FINE_TABLE)],
    }


# ---------------------------------------------------------------------------------------------
# Bursts and shocks, from flow.json.


def bursts(flow):
    """The two rate-multiplier models of `--bursts`: `exp(fast + slow)`, each an AR(1) process
    stepped once a second, fitted to the per-second message rate of the median hour and of the
    busiest hour (flow.json, `burstiness.rate_multiplier_models`). The schedule's floats."""
    models = flow["burstiness"]["rate_multiplier_models"]

    def pair(model):
        return [{"phi": c["phi"], "innovation_sd": c["innovation_sd"]} for c in model["components"]]

    return {"median": pair(models["median_hour_2ar1"]), "busiest": pair(models["busiest_hour_2ar1"])}


def shocks(flow):
    """Correlated shocks (flow.json, `correlated_shocks`, the price detector on clean mids):
    how often, how many markets, how far each moves, and how close together.

    Markets per shock: the detector's discrete Pareto (alpha 1.41, fitted above x_min 8), with
    D-034's "at least 14, at most all 88" read as a clamp: a draw below 14 moves 14 markets,
    one beyond 88 all 88. That gives a median of 14 (recorded: 14), a 90th percentile of 38
    (recorded: 31) and 4.1% of shocks beyond the recorded maximum of 71 (3.1% move all 88),
    since the Pareto's tail is a little heavier than the 227 recorded events. Read instead as
    conditioned on at least 14, the Pareto would give a median of 22 and a 90th percentile of
    69, twice the recording's."""
    detector = flow["correlated_shocks"]["price_detector"]["mid"]
    alpha = detector["movers_pareto_alpha_xmin8"]
    x_min, least, most = 8, 14, 88

    def at_least(n):  # P(N >= n) of the Pareto above x_min, with a half-market correction
        return ((n - 0.5) / (x_min - 0.5)) ** -alpha

    # 14 takes every draw up to 14, and 88 every draw from 88 on.
    markets = [
        (1 if n == least else at_least(n)) - (at_least(n + 1) if n < most else 0)
        for n in range(least, most + 1)
    ]
    sigma = detector["move_sigma_units_median_mover"]
    bps = detector["abs_move_bps_median_mover"]
    span = detector["first_print_span80_ms_single_second_events"]
    return {
        "pooled_every_s": round(3600 / detector["per_hour"]),
        "calm_every_s": round(3600 / detector["events_per_hour_median_over_full_hours"]),
        "markets_pareto_alpha_milli": milli(alpha),
        "min_markets": least,
        "max_markets": most,
        "markets_ppm": ppm_split(markets),
        "move_sigmas_p50_milli": milli(sigma["p50"]),
        "move_sigmas_p90_milli": milli(sigma["p90"]),
        "move_centibps_p10_p50_p90": [round(bps[p] * 100) for p in ("p10", "p50", "p90")],
        "stress_move_ppm": [20_000, 60_000],
        "spread_ms_p50": round(span["p50"]),
        "spread_ms_p90": round(span["p90"]),
    }
