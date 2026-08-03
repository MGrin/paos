#!/usr/bin/env python3
"""difflib parity: sample real fact pairs, then compare CPython's ratio with the Rust port.

`_supersede_candidates` uses difflib.SequenceMatcher(...).ratio(), and librarian.draft()
BRANCHES on it — at or above the threshold a fact is queued as a `supersede` proposal
instead of a `capture`. So this number decides what a human is asked to approve, and an
approximate port changes the proposal rather than a warning string.

  build   — sample pairs from the live store into a JSON file
  ratios  — print CPython's ratio for each pair, one per line

The Rust side is `cargo run --release --bin difflib-parity < pairs.json`. Both read the
SAME pair file, so a difference is the algorithm and not the sampling.
"""
import difflib
import json
import os
import random
import sqlite3
import sys


def _esc(s):
    return s.replace("\\", "\\\\").replace("\n", "\\n").replace("\r", "\\r")


def _unesc(s):
    out, i = [], 0
    while i < len(s):
        if s[i] == "\\" and i + 1 < len(s):
            nxt = s[i + 1]
            out.append({"n": "\n", "r": "\r", "\\": "\\"}.get(nxt, nxt))
            i += 2
        else:
            out.append(s[i])
            i += 1
    return "".join(out)


def _read_pairs(path):
    with open(path, "r", encoding="utf-8") as fh:
        lines = fh.read().split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    return [(_unesc(lines[i]), _unesc(lines[i + 1])) for i in range(0, len(lines), 2)]


def build(out_path, n_pairs, seed=20260731):
    db = os.path.expanduser(os.environ.get("PAOS_DB", "~/.paos/paos.db"))
    con = sqlite3.connect("file:%s?mode=ro" % db, uri=True)
    con.row_factory = sqlite3.Row
    rows = con.execute(
        "SELECT dataset, text FROM memories WHERE superseded IS NULL AND text <> ''"
    ).fetchall()
    con.close()
    by_ds = {}
    for r in rows:
        by_ds.setdefault(r["dataset"], []).append(r["text"])

    rnd = random.Random(seed)
    pairs = []
    # Same-dataset pairs are what _supersede_candidates actually compares.
    datasets = [d for d, v in by_ds.items() if len(v) > 1]
    while len(pairs) < n_pairs and datasets:
        ds = rnd.choice(datasets)
        texts = by_ds[ds]
        a, b = rnd.sample(texts, 2)
        pairs.append((a, b))
    # Deliberately include the shapes most likely to expose an autojunk mistake:
    # a long repetitive string, an exact self-match, empties, and multi-byte text.
    longest = max((t for v in by_ds.values() for t in v), key=len, default="x")
    pairs += [
        (longest, longest),
        (longest, longest[: len(longest) // 2]),
        ("", ""),
        ("", longest[:50]),
        ("日本語のテスト", "日本語のテスト"),
        ("日本語のテスト", "日本語"),
        ("ab" * 150, "ab" * 150),
        ("ab" * 150, "ba" * 150),
        ("x" * 500, "x" * 499 + "y"),
    ]
    # Two escaped lines per pair. Deliberately not JSON: the Rust side lives in
    # paos-memory, which ships inside paosd, and a parity harness is not a reason to add
    # a dependency to it.
    with open(out_path, "w", encoding="utf-8") as fh:
        for a, b in pairs:
            fh.write(_esc(a) + "\n" + _esc(b) + "\n")
    lens = sorted(len(a) + len(b) for a, b in pairs)
    over = sum(1 for a, b in pairs if len(b) >= 200)
    print("pairs: %d  ·  autojunk-eligible (len(b)>=200): %d  ·  median combined len: %d"
          % (len(pairs), over, lens[len(lens) // 2]))


def ratios(path):
    pairs = _read_pairs(path)
    out = []
    for a, b in pairs:
        out.append("%.17f" % difflib.SequenceMatcher(None, a, b).ratio())
    sys.stdout.write("\n".join(out) + "\n")


if __name__ == "__main__":
    if len(sys.argv) >= 2 and sys.argv[1] == "build":
        build(sys.argv[2], int(sys.argv[3]) if len(sys.argv) > 3 else 300)
    elif len(sys.argv) >= 2 and sys.argv[1] == "ratios":
        ratios(sys.argv[2])
    else:
        sys.stderr.write(__doc__)
        sys.exit(2)
