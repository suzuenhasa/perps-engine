#!/usr/bin/env python3
"""Calibrates the Polymarket-shaped flow (docs/DECISIONS.md D-034) from the recorded data.

    python3 tools/calibrate/calibrate.py extract [--check]   # data/ + inputs/ -> profile JSON
    python3 tools/calibrate/calibrate.py generate [--check]  # profile JSON -> Rust table

`extract` reads the recorded sample (data/, never committed) and the copied calibration outputs
(inputs/), and writes tools/calibrate/profile-2026-09-30.json: derived parameters only. It takes
about 3 minutes on 6 processes. `generate` renders that JSON into
loadgen/src/market_flow/polymarket_profile.rs, in a second. With `--check`, each writes nothing
and fails if the file it would write differs from the one in the repo: the Rust table must be
what the tool makes from the committed JSON, byte for byte.

Standard library only; see README.md for the sample, the method and the corrections applied.
"""

import argparse
import hashlib
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)
sys.dont_write_bytecode = True  # no __pycache__ in the repo

import derive  # noqa: E402
import rust  # noqa: E402
import scan  # noqa: E402

PROFILE = os.path.join(HERE, "profile-2026-09-30.json")
RUST = os.path.join(REPO, "loadgen", "src", "market_flow", "polymarket_profile.rs")
INPUTS = ["inputs/flow.json", "inputs/book.json"]

NAME = "Polymarket Perps, 2026-09-30"
SAMPLE = "recorded 2026-09-29 14:00 to 2026-09-30 15:59 UTC; start prices at 2026-09-30 10:00:00"


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


# ---------------------------------------------------------------------------------------------
# A stable, readable JSON layout: one key per line, short lists and objects inline.


def to_json(value, indent=0):
    """`value` as JSON: an object or list that fits in 100 columns on one line stays on it; a
    longer list of numbers wraps; anything else takes one entry per line."""
    inline = json.dumps(value)
    pad, inner = "  " * indent, "  " * (indent + 1)
    if len(inline) + len(pad) <= 100 or not isinstance(value, (dict, list)):
        return inline
    if isinstance(value, dict):
        entries = ["%s%s: %s" % (inner, json.dumps(k), to_json(v, indent + 1)) for k, v in value.items()]
        return "{\n%s\n%s}" % (",\n".join(entries), pad)
    if all(isinstance(v, (int, float)) for v in value):
        lines, line = [], ""
        for v in value:
            item = json.dumps(v) + ","
            if line and len(inner) + len(line) + 1 + len(item) > 100:
                lines.append(inner + line)
                line = item
            else:
                line = (line + " " + item) if line else item
        lines.append(inner + line[:-1])  # no comma after the last number
        return "[\n%s\n%s]" % ("\n".join(lines), pad)
    return "[\n%s\n%s]" % (",\n".join(inner + to_json(v, indent + 1) for v in value), pad)


# ---------------------------------------------------------------------------------------------
# The two steps.


def extract(data_dir):
    """The profile's JSON text: scan the data, derive the parameters, add provenance."""
    scanned = scan.scan(data_dir, scan.default_processes())
    with open(os.path.join(HERE, "inputs", "flow.json")) as f:
        flow = json.load(f)
    with open(os.path.join(HERE, "inputs", "book.json")) as f:
        book = json.load(f)
    about = {
        "name": NAME,
        "sample": SAMPLE,
        "made_by": "python3 tools/calibrate/calibrate.py extract (tools/calibrate/README.md)",
        "sha256": dict(
            [(scan.INSTRUMENTS_FILE, sha256(os.path.join(data_dir, scan.INSTRUMENTS_FILE)))]
            + [(name, sha256(os.path.join(HERE, name))) for name in INPUTS]
        ),
    }
    profile = dict(about=about, **derive.derive(scanned, flow, book))
    return to_json(profile) + "\n"


def compare_or_write(path, text, check):
    """Writes `text` to `path`, or with `check` compares them and reports the first difference."""
    shown = os.path.relpath(path, REPO)
    if not check:
        with open(path, "w") as f:
            f.write(text)
        print("wrote %s" % shown)
        return 0
    with open(path) as f:
        current = f.read()
    if current == text:
        print("%s is what the tool makes: identical" % shown)
        return 0
    for n, (a, b) in enumerate(zip(current.splitlines(), text.splitlines()), 1):
        if a != b:
            print("%s differs at line %d:\n  in the repo: %s\n  from the tool: %s" % (shown, n, a, b))
            break
    else:
        print("%s differs in length" % shown)
    return 1


def main():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("step", choices=["extract", "generate"])
    parser.add_argument(
        "--check", action="store_true", help="compare with the file in the repo; write nothing"
    )
    parser.add_argument(
        "--data", default=os.path.join(REPO, "data"), help="the recorded data (default: data/)"
    )
    args = parser.parse_args()
    if args.step == "extract":
        return compare_or_write(PROFILE, extract(args.data), args.check)
    with open(PROFILE, "rb") as f:
        raw = f.read()
    # The table carries the JSON's SHA-256, which the flow's digest hashes: a regenerated
    # table is a new plan's.
    rendered = rust.render(json.loads(raw), hashlib.sha256(raw).hexdigest())
    return compare_or_write(RUST, rendered, args.check)


if __name__ == "__main__":
    sys.exit(main())
