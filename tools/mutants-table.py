#!/usr/bin/env python3
# prints the mutants/README.md result rows and the totals line from each scope's summary.txt
import re
import sys
from pathlib import Path

LINE = re.compile(r"(\d+) mutants tested in (\S+): (.*)")
COUNT = re.compile(r"(\d+) (missed|caught|unviable|timeouts?)")


def read_summary(path):
    text = path.read_text()
    match = LINE.search(text)
    if not match:
        raise SystemExit(f"{path}: no totals line")
    total, wall, rest = match.groups()
    counts = {name.rstrip("s"): int(value) for value, name in COUNT.findall(rest)}
    return int(total), wall, counts


def score(counts):
    caught = counts.get("caught", 0)
    missed = counts.get("missed", 0)
    return round(100 * caught / (caught + missed))


def main(root):
    sums = {"caught": 0, "missed": 0, "timeout": 0, "unviable": 0}
    total_all = 0
    for summary in sorted(root.glob("g2g-plugins-*/summary.txt")):
        total, wall, counts = read_summary(summary)
        scope = summary.parent.name.removeprefix("g2g-plugins-")
        total_all += total
        for key in sums:
            sums[key] += counts.get(key, 0)
        print(
            f"| g2g-plugins {scope}.rs | {total} | {counts.get('caught', 0)} | {counts.get('missed', 0)}"
            f" | {counts.get('timeout', 0)} | {counts.get('unviable', 0)} | {score(counts)}% | {wall} |"
        )
    print(
        f"g2g-plugins total: {total_all} mutants, {sums['caught']} caught, {sums['missed']} missed,"
        f" {sums['timeout']} timeout, {sums['unviable']} unviable, score {score(sums)}%"
    )


if __name__ == "__main__":
    main(Path(sys.argv[1] if len(sys.argv) > 1 else "mutants"))
