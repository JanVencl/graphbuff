"""Runs every graphbuff benchmark and saves the output.

    python benchmarks/run_all.py                 all benchmarks, default sizes
    python benchmarks/run_all.py --quick         small graphs (a smoke run, a few minutes)
    python benchmarks/run_all.py --only adjbench txbench
    python benchmarks/run_all.py --no-gpu        skip gpubench and vecbench (need --features gpu)
    python benchmarks/run_all.py --list          list the benchmarks

embedbench measures the model in GRAPHBUFF_EMBED_BENCH_MODEL (a model folder, see
docs/Embeddings.md); without it, the tiny test model (overhead only).

The benchmarks are the release-built examples in crates/engine/examples (embedbench: in
crates/embed/examples). Each one's output
is printed live and written to benchmarks/results/<timestamp>/<name>.txt. Exits 1 if one fails.
"""

import argparse
import os
import subprocess
import sys
import time
from datetime import datetime

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
RESULTS = os.path.join(HERE, "results")

# name -> (description, cargo features, default args, quick args)
BENCHMARKS = {
    "adjbench": ("traversal queries per storage backend", [],
                 [], ["--likes", "100000", "--people", "10000", "--reps", "3"]),
    "csrbench": ("CSR export and CPU kernels", [],
                 [], ["--people", "50000", "--reps", "3"]),
    "txbench": ("write transactions, read latency during writes", [],
                [], ["20000"]),
    "gpubench": ("GPU kernels vs CPU kernels", ["gpu"],
                 [], ["--people", "50000", "--reps", "3"]),
    "vecbench": ("vector index queries vs ORDER BY similarity, CPU and GPU", ["gpu"],
                 [], ["--docs", "20000", "--dims", "128", "--reps", "3"]),
    "embedbench": ("text embedding throughput by batch size and length (CPU)", [],
                   [], ["--quick", "--reps", "1"]),
}

# The crate of each benchmark that is not in crates/engine.
PACKAGES = {"embedbench": "graphbuff-embed"}


def extra_args(name):
    if name == "embedbench" and os.environ.get("GRAPHBUFF_EMBED_BENCH_MODEL"):
        return ["--model", os.environ["GRAPHBUFF_EMBED_BENCH_MODEL"]]
    return []


def cargo(name, features, args):
    cmd = ["cargo", "run", "--release", "--quiet", "-p", PACKAGES.get(name, "graphbuff"), "--example", name]
    if features:
        cmd += ["--features", ",".join(features)]
    return cmd + (["--", *args] if args else [])


def run(name, cmd, log_path):
    """Runs `cmd`, echoing its output and writing it to `log_path`. Returns the exit code."""
    with open(log_path, "w", encoding="utf-8", newline="\n") as log:
        log.write("$ " + " ".join(cmd) + "\n\n")
        proc = subprocess.Popen(cmd, cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                text=True, encoding="utf-8", errors="replace", bufsize=1)
        for line in proc.stdout:
            sys.stdout.write(line)
            sys.stdout.flush()
            log.write(line)
        return proc.wait()


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--quick", action="store_true", help="small graphs")
    ap.add_argument("--only", nargs="+", choices=BENCHMARKS, metavar="NAME", help="run only these")
    ap.add_argument("--no-gpu", action="store_true", help="skip benchmarks that need the gpu feature")
    ap.add_argument("--list", action="store_true", help="list the benchmarks and exit")
    args = ap.parse_args()

    if args.list:
        for name, (desc, features, _, _) in BENCHMARKS.items():
            print(f"{name:<10} {desc}" + (f"  [features: {', '.join(features)}]" if features else ""))
        return

    names = args.only or list(BENCHMARKS)
    if args.no_gpu:
        names = [n for n in names if "gpu" not in BENCHMARKS[n][1]]

    out_dir = os.path.join(RESULTS, datetime.now().strftime("%Y%m%d-%H%M%S") + ("-quick" if args.quick else ""))
    os.makedirs(out_dir, exist_ok=True)

    summary = []
    for name in names:
        desc, features, default_args, quick_args = BENCHMARKS[name]
        cmd = cargo(name, features, (quick_args if args.quick else default_args) + extra_args(name))
        print(f"\n===== {name}: {desc}\n$ {' '.join(cmd)}\n", flush=True)
        start = time.time()
        code = run(name, cmd, os.path.join(out_dir, f"{name}.txt"))
        summary.append((name, code, time.time() - start))

    print(f"\n===== summary (output in {os.path.relpath(out_dir, ROOT)})")
    for name, code, secs in summary:
        print(f"{'PASS' if code == 0 else 'FAIL':<5} {name:<10} {secs:7.1f} s" + (f"  exit {code}" if code else ""))
    if any(code for _, code, _ in summary):
        sys.exit(1)


if __name__ == "__main__":
    main()
