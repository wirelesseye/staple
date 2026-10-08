#!/usr/bin/env python3
"""Compile the GC product benchmark, then report execution-only timings.

The program retains 100,000 products holding strings through collections and
verifies every string, so a run only counts if no live data was freed.
"""
import argparse
from pathlib import Path
import statistics
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--compiler", required=True)
parser.add_argument("--runs", type=int, default=5)
args = parser.parse_args()
root = Path(__file__).resolve().parents[1]
with tempfile.TemporaryDirectory(prefix="staple-gc-bench-") as directory:
    executable = Path(directory) / "bench"
    subprocess.run([str(Path(args.compiler).resolve()), "compile", "--stdlib", str(root / "stdlib"),
                    "-o", str(executable), str(root / "scripts/bench-gc-products.sta")], check=True)
    timings = []
    for _ in range(args.runs):
        start = time.perf_counter()
        result = subprocess.run([str(executable)], capture_output=True, check=True)
        timings.append(time.perf_counter() - start)
        assert result.stdout == b"100000 100000\n", result.stdout
    print(f"seconds: {timings}; median: {statistics.median(timings):.6f}")
