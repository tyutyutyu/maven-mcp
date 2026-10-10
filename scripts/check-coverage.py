#!/usr/bin/env python3
"""Validate a `cargo llvm-cov report --json --summary-only` file.

Usage: check-coverage.py REPORT_JSON [THRESHOLD_PERCENT]

The line coverage threshold is checked on the exact covered/total counts, so
no rounding can lift a value below the threshold.
"""
import json
import sys


def fail(message):
    print(f"Coverage gate error: {message}", file=sys.stderr)
    sys.exit(1)


def main(argv):
    if len(argv) not in (2, 3):
        fail("usage: check-coverage.py REPORT_JSON [THRESHOLD_PERCENT]")
    try:
        threshold = int(argv[2]) if len(argv) == 3 else 80
        with open(argv[1], encoding="utf-8") as report:
            lines = json.load(report)["data"][0]["totals"]["lines"]
        count, covered = lines["count"], lines["covered"]
    except (OSError, ValueError, KeyError, IndexError, TypeError) as error:
        fail(f"missing or unreadable coverage report {argv[1]}: {error!r}")
    if type(count) is not int or type(covered) is not int or not 0 <= covered <= count:
        fail(f"invalid line counts in report: covered={covered!r}, count={count!r}")
    if count == 0:
        fail("the report contains zero measurable lines")
    print(
        f"Rust line coverage: {covered * 100 / count:.2f}% "
        f"({covered}/{count} lines), threshold {threshold}%"
    )
    if covered * 100 < threshold * count:
        fail(f"line coverage is below the {threshold}% threshold")


main(sys.argv)
