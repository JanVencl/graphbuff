"""Runs the openCypher TCK against graphbuff-server and reports the pass rate.

    python run_tck.py                   run everything; print the pass rate per feature group
    python run_tck.py --features        ... a line per feature file
    python run_tck.py --failures        ... and every failing scenario with the reason
    python run_tck.py --report FILE     ... and write the table as Markdown to FILE
    python run_tck.py --save-baseline   ... and record the passing scenarios in tck/passing.txt
    python run_tck.py -v                show every scenario live (default: a line per feature file)
    python run_tck.py -q                no live progress
    python run_tck.py -- ARGS           pass ARGS to behave, run from integration-tests/
                                        (e.g. tck/features/clauses/match/Match1.feature)

Exits 1 if a scenario listed in tck/passing.txt no longer passes (on a full run).
"""

import argparse
import os
import subprocess
import sys
import time
from collections import defaultdict

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))  # integration-tests/
TCK = os.path.join(TESTS, "tck")
RESULTS = os.path.join(TCK, "results", "results.tsv")
BASELINE = os.path.join(TCK, "passing.txt")
TCK_VERSION = "2024.3"


def read_results():
    with open(RESULTS, encoding="utf-8") as f:
        return [line.rstrip("\n").split("\t") for line in f if line.strip()]


def scenario_id(feature, name):
    return f"{feature.rsplit('/', 1)[-1]} {name}"


def markdown(results, per_feature):
    table = defaultdict(lambda: [0, 0])
    skipped = 0
    for feature, _, status, _ in results:
        if status == "skipped":
            skipped += 1
            continue
        key = feature if per_feature else feature.rsplit("/", 1)[0]
        table[key][1] += 1
        table[key][0] += status == "passed"
    passed = sum(p for p, _ in table.values())
    total = sum(t for _, t in table.values())
    pct = lambda p, t: 100.0 * p / t if t else 0.0  # noqa: E731
    lines = [f"openCypher TCK {TCK_VERSION}: {passed} of {total} scenarios pass ({pct(passed, total):.1f} %)"]
    if skipped:
        lines.append(f"({skipped} not run: marked @ignore by the TCK)")
    lines += ["", f"| {'Feature' if per_feature else 'Features'} | Passed | Total | % |", "|---|---:|---:|---:|"]
    for k in sorted(table):
        p, t = table[k]
        lines.append(f"| {k} | {p} | {t} | {pct(p, t):.0f} |")
    return "\n".join(lines) + "\n"


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--features", action="store_true")
    ap.add_argument("--failures", action="store_true")
    ap.add_argument("--report")
    ap.add_argument("--save-baseline", action="store_true")
    ap.add_argument("-v", "--verbose", action="store_true", help="a live line per scenario")
    ap.add_argument("-q", "--quiet", action="store_true", help="no live progress")
    ap.add_argument("behave_args", nargs="*")
    args = ap.parse_args()
    full = not args.behave_args
    if args.save_baseline and not full:
        ap.error("--save-baseline needs a full run")

    if os.path.exists(RESULTS):
        os.remove(RESULTS)
    start = time.time()
    env = dict(os.environ, TCK_PROGRESS="" if args.quiet else "scenario" if args.verbose else "feature")
    paths = args.behave_args or ["tck/features"]
    subprocess.run([sys.executable, "-m", "behave", "-f", "null", "--no-summary", *paths],
                   cwd=TESTS, env=env)
    if not args.quiet:
        print()
    elapsed = time.time() - start
    if not os.path.exists(RESULTS):
        sys.exit("behave wrote no results")
    results = read_results()

    if args.failures:
        for feature, name, status, reason in results:
            if status == "failed":
                print(f"FAIL {scenario_id(feature, name)}\n    {reason}\n")

    md = markdown(results, args.features)
    print(md)
    print(f"{len(results)} scenarios in {elapsed:.0f} s")
    if args.report:
        with open(args.report, "w", encoding="utf-8", newline="\n") as f:
            f.write(md)

    passing = sorted(scenario_id(f, n) for f, n, s, _ in results if s == "passed")
    if args.save_baseline:
        with open(BASELINE, "w", encoding="utf-8", newline="\n") as f:
            f.write(f"# openCypher TCK {TCK_VERSION} scenarios that pass; python/run_tck.py fails if one stops passing.\n"
                    "# Regenerate with: python python/run_tck.py --save-baseline\n")
            f.write("".join(p + "\n" for p in passing))
        print(f"wrote {BASELINE}")
    elif full and os.path.exists(BASELINE):
        with open(BASELINE, encoding="utf-8") as f:
            baseline = {line.strip() for line in f if line.strip() and not line.startswith("#")}
        now = set(passing)
        lost = sorted(baseline - now)
        gained = len(now - baseline)
        if gained:
            print(f"{gained} scenarios pass that are not in passing.txt yet (python python/run_tck.py --save-baseline)")
        if lost:
            print(f"{len(lost)} scenarios in passing.txt no longer pass:")
            print("".join(f"  {x}\n" for x in lost))
            sys.exit(1)


if __name__ == "__main__":
    main()
