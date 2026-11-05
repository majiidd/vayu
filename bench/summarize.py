#!/usr/bin/env python3
"""Summarize bench/scenarios.sh output as the median table used in docs/BENCHMARKS.md.

Usage: bench/scenarios.sh 3 | tee bench.txt; bench/summarize.py bench.txt

For each scenario: the median seconds of vayu, vayu-1, and curl, plus vayu's request count,
peak connections, and bytes sent beyond the file size in its median run. A digest that differs
between runs of a scenario is reported, since every tool must produce the same file.
"""

import re
import statistics
import sys

HEADER = re.compile(r"^== (\S+) \((.*)\)$")
ROW = re.compile(r"^(\S+)\s+([\d.]+|FAILED)(?:\s+(\S+)\s+(\S+)\s*(.*))?$")
SIZE = re.compile(r"--size (\d+)")


def parse(lines):
    scenarios = []
    for line in lines:
        line = line.rstrip("\n")
        if m := HEADER.match(line):
            size = SIZE.search(m.group(2))
            scenarios.append(
                {"name": m.group(1), "size": int(size.group(1)) << 20 if size else None, "runs": []}
            )
        elif scenarios and (m := ROW.match(line)) and m.group(1) != "tool":
            stats = dict(kv.split("=", 1) for kv in (m.group(5) or "").split())
            scenarios[-1]["runs"].append(
                {
                    "tool": m.group(1),
                    "seconds": None if m.group(2) == "FAILED" else float(m.group(2)),
                    "digest": m.group(4),
                    "stats": stats,
                }
            )
    return scenarios


def kib(n):
    return "0" if n == 0 else f"{n / 1024:.0f} KiB"


def main():
    source = open(sys.argv[1]) if len(sys.argv) > 1 else sys.stdin
    print("| Scenario | vayu | vayu-1 | curl | Requests | Peak connections | Sent - size |")
    print("|---|---:|---:|---:|---:|---:|---:|")
    problems = []
    for s in parse(source):
        cells = []
        for tool in ("vayu", "vayu-1", "curl"):
            times = [r["seconds"] for r in s["runs"] if r["tool"] == tool and r["seconds"] is not None]
            failed = sum(1 for r in s["runs"] if r["tool"] == tool and r["seconds"] is None)
            if failed:
                problems.append(f"{s['name']}: {tool} failed {failed} time(s)")
            cells.append(f"{statistics.median(times):.2f} s" if times else "-")
        digests = {r["digest"] for r in s["runs"] if r["digest"]}
        if len(digests) > 1:
            problems.append(f"{s['name']}: outputs differ ({', '.join(sorted(digests))})")
        vayu = sorted(
            (r for r in s["runs"] if r["tool"] == "vayu" and r["seconds"] is not None),
            key=lambda r: r["seconds"],
        )
        requests = peak = extra = "-"
        if vayu and vayu[0]["stats"]:
            median = vayu[(len(vayu) - 1) // 2]["stats"]
            requests, peak = median.get("requests", "-"), median.get("peak_conns", "-")
            if s["size"] and "sent" in median:
                extra = kib(int(median["sent"]) - s["size"])
        print(f"| `{s['name']}` | {' | '.join(cells)} | {requests} | {peak} | {extra} |")
    for problem in problems:
        print(f"\nPROBLEM: {problem}", file=sys.stderr)
    sys.exit(1 if problems else 0)


if __name__ == "__main__":
    main()
