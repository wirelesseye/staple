#!/usr/bin/env python3
"""Time `staple expand` end to end on three synthetic inputs, best of N.

  B1  one trivial binding (cost of loading and expanding the standard library)
  B2  20 x `@derive_eq` on a 50-field `wrap` product
  B3  one function with 1,000 `if ... else ...`
"""
import argparse
from pathlib import Path
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]


def sources():
    fields = ", ".join(f"f{i}: I32" for i in range(50))
    return {
        "B1": "let x: I32 = 1\n",
        "B2": "use std.cmp.(derive_eq)\n" + "".join(f"@derive_eq\ntype P{k} wrap ({fields})\n" for k in range(20)),
        "B3": "def f = (x: I32) => I32 :: {\n    let mut y: I32 = 0\n"
              + "".join(f"    if (x > {i}) {{ y = y + 1 }} else {{ y = y - 1 }}\n" for i in range(1000))
              + "    y\n}\n",
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--staple", default=str(ROOT / "target/release/staple"))
    parser.add_argument("--runs", type=int, default=5)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="staple-bench-") as directory:
        for name, source in sources().items():
            path = Path(directory) / f"{name}.sta"
            path.write_text(source)
            best = float("inf")
            for _ in range(args.runs):
                started = time.perf_counter()
                result = subprocess.run([args.staple, "expand", "--stdlib", str(ROOT / "stdlib"), str(path)],
                                        capture_output=True)
                best = min(best, time.perf_counter() - started)
                if result.returncode:
                    raise RuntimeError(f"{name}: {result.stderr.decode()[:300]}")
            print(f"{name}: {best:.3f} s")


if __name__ == "__main__":
    main()
