#!/usr/bin/env python3
"""Combines result files of `bohio-bench`.

    summarize.py merge --out FILE ROUND.json...     one result from the rounds of one build
    summarize.py ab A.json B.json                   a table of two merged results, as compare-bench reads them

A metric's median over rounds is the median of the rounds' medians; its min and max are the smallest
and the largest seen. The change is (B - A) / A on that median; a hosted runner cannot show a gain
under about 10%, so read a smaller one as noise, and read the controls (`raw_*`, `cpu_sha256_*`) first.
"""
import json
import statistics
import sys


def load(path):
    with open(path, encoding="utf-8") as handle:
        return json.load(handle)


def merge(out, paths):
    results = [load(path) for path in paths]
    merged = {"name": results[0]["name"], "raw": dict(results[0]["raw"]), "metrics": {}}
    merged["raw"]["label"] = results[0]["raw"].get("label", "")
    names = sorted({name for result in results for name in result["metrics"]})
    for name in names:
        rows = [result["metrics"][name] for result in results if name in result["metrics"]]
        row = {
            "min_ms": min(r["min_ms"] for r in rows),
            "median_ms": statistics.median(r["median_ms"] for r in rows),
            "max_ms": max(r["max_ms"] for r in rows),
            "samples": sum(r["samples"] for r in rows),
        }
        if "bytes" in rows[0]:
            row["bytes"] = rows[0]["bytes"]
        merged["metrics"][name] = row
    curves = {}
    for result in results:
        for name, lists in result.get("curves", {}).items():
            curves.setdefault(name, []).extend(lists)
    if curves:
        merged["curves"] = curves
    with open(out, "w", encoding="utf-8", newline="\n") as handle:
        json.dump(merged, handle, indent=2)
        handle.write("\n")


def ab(a_path, b_path):
    a, b = load(a_path), load(b_path)
    width = max((len(name) for name in a["metrics"]), default=10)
    print(f"A {a['raw'].get('commit', '')}   B {b['raw'].get('commit', '')}   link {a['raw'].get('link', '')}")
    print(f"{'metric':<{width}} {'A median ms':>12} {'B median ms':>12} {'change':>8}  {'A MB/s':>8} {'B MB/s':>8}")
    for name in sorted(a["metrics"]):
        if name not in b["metrics"]:
            continue
        am, bm = a["metrics"][name], b["metrics"][name]
        change = (bm["median_ms"] - am["median_ms"]) / am["median_ms"] * 100 if am["median_ms"] else 0.0

        def rate(row):
            if "bytes" in row and row["median_ms"]:
                return f"{row['bytes'] / row['median_ms'] / 1000:8.2f}"
            return " " * 8

        print(f"{name:<{width}} {am['median_ms']:12.1f} {bm['median_ms']:12.1f} {change:+7.1f}%  {rate(am)} {rate(bm)}")


def main(argv):
    if len(argv) >= 4 and argv[1] == "merge" and argv[2] == "--out":
        merge(argv[3], argv[4:])
    elif len(argv) == 4 and argv[1] == "ab":
        ab(argv[2], argv[3])
    else:
        print(__doc__)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
