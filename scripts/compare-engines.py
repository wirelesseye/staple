#!/usr/bin/env python3
"""Expand every input with both macro engines and report where they differ.

  compare-engines.py [--staple PATH] [--test-sources DIR | --dump-tests] [--out DIR] [--only PATTERN]

Inputs are the ones `compare-expansion.py` uses (the standard library's entry
files, the examples, the compiler test fixtures and the corpus), plus, with
`--dump-tests` or `--test-sources`, every program the compiler's integration
tests load from a string (`STAPLE_SOURCE_DUMP`).

Each input is expanded with `staple expand --tree-dump` under
`--macro-engine=interp` and `--macro-engine=compiled`: the expanded tree as
`macro_runtime::canonical::tree_dump` writes it, with the differences Phase 3
found normalized. Each is also checked with `staple check` under both engines,
which resolves and type-checks the expansion. An input matches when both
engines give the same exit status, the same tree and the same diagnostics.
Mismatches are written under `--out` with both sides. The script also counts
the inputs whose output says a macro "could not be run as compiled code", which
is a compiler fault, and exits non-zero when there is one.

The compiled engine shares one macro cache, warmed on a trivial program first
so the standard library's libraries are built once.
"""
import argparse
import difflib
import importlib.util
import json
import os
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
import re
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]

spec = importlib.util.spec_from_file_location("compare_expansion", ROOT / "scripts/compare-expansion.py")
compare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(compare)


def dump_test_sources(destination):
    destination.mkdir(parents=True, exist_ok=True)
    environment = {**os.environ, "STAPLE_SOURCE_DUMP": str(destination)}
    # Failing tests still load their sources; the comparison is what matters.
    subprocess.run(["cargo", "test", "--quiet", "-p", "staple-compiler", "--test", "*", "--no-fail-fast"],
                   cwd=ROOT, capture_output=True, env=environment)


def run(binary, arguments, source, cache, timeout):
    environment = {**os.environ, "XDG_CACHE_HOME": str(cache)}
    environment.pop("STAPLE_MACRO_STATS", None)
    try:
        result = subprocess.run([str(binary), *arguments, "--stdlib", str(ROOT / "stdlib"), str(source)],
                                capture_output=True, timeout=timeout, env=environment)
    except subprocess.TimeoutExpired:
        return dict(status="timeout", stdout="", stderr="")
    stderr = result.stderr.decode(errors="replace").replace(str(source), "<input>")
    # A crash names the thread by its process-specific number.
    stderr = re.sub(r"(thread '[^']*') \(\d+\)", r"\1", stderr)
    return dict(status=result.returncode, stdout=compare.normalize(result.stdout.decode(errors="replace")),
                stderr=compare.normalize(stderr))


def both(binary, source, cache, timeout):
    sides = {}
    for engine in ["interp", "compiled"]:
        sides[engine] = dict(
            expand=run(binary, ["expand", "--tree-dump", f"--macro-engine={engine}"], source, cache, timeout),
            check=run(binary, ["check", f"--macro-engine={engine}"], source, cache, timeout))
    return sides


def differences(sides):
    found = []
    for step in ["expand", "check"]:
        left, right = sides["interp"][step], sides["compiled"][step]
        if left["status"] != right["status"]:
            found.append(f"{step} status")
        if left["stdout"] != right["stdout"]:
            found.append(f"{step} output")
        if left["stderr"] != right["stderr"]:
            found.append(f"{step} diagnostics")
    return found


REFUSED = "could not be run as compiled code"


def refused(sides):
    """Whether any output says compiled code could not run a macro. That is a
    compiler fault wherever it appears, so it is counted apart from differences."""
    return any(REFUSED in sides[engine][step][stream]
               for engine in sides for step in ["expand", "check"] for stream in ["stdout", "stderr"])


def write_mismatch(out, name, sides, found):
    target = out / name
    target.mkdir(parents=True, exist_ok=True)
    (target / "differences.json").write_text(json.dumps(found, indent=2) + "\n")
    for step in ["expand", "check"]:
        for stream in ["stdout", "stderr"]:
            left, right = sides["interp"][step][stream], sides["compiled"][step][stream]
            if left != right:
                diff = difflib.unified_diff(left.splitlines(), right.splitlines(), "interp", "compiled",
                                            lineterm="", n=8)
                (target / f"{step}.{stream}.diff").write_text("\n".join(diff) + "\n")
        (target / f"{step}.status").write_text(
            f"interp {sides['interp'][step]['status']}, compiled {sides['compiled'][step]['status']}\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--staple", help="binary to run; default builds the release binary")
    parser.add_argument("--test-sources", help="directory of sources dumped from the integration tests")
    parser.add_argument("--dump-tests", action="store_true", help="dump the integration tests' sources first")
    parser.add_argument("--cache", help="macro cache root (XDG_CACHE_HOME); default a temporary directory")
    parser.add_argument("--out", default=str(ROOT / "target/compare-engines"), help="where mismatches go")
    parser.add_argument("--only", help="regular expression an input's name must match")
    parser.add_argument("--jobs", type=int, default=max(1, (os.cpu_count() or 2) // 2))
    parser.add_argument("--timeout", type=float, default=300)
    args = parser.parse_args()

    binary = Path(args.staple).resolve() if args.staple else compare.build(ROOT)
    with tempfile.TemporaryDirectory(prefix="staple-engines-") as directory:
        directory = Path(directory)
        cache = Path(args.cache) if args.cache else directory / "cache"
        inputs = compare.collect_inputs(directory / "corpus")
        tests = Path(args.test_sources) if args.test_sources else None
        if args.dump_tests:
            tests = tests or directory / "tests"
            dump_test_sources(tests)
        if tests:
            inputs.update({f"tests/{path.name}": path for path in sorted(tests.glob("*.sta"))})
        if args.only:
            inputs = {name: path for name, path in inputs.items() if re.search(args.only, name)}

        warm = directory / "warm.sta"
        warm.write_text("let x: I32 = 1\n")
        run(binary, ["check", "--macro-engine=compiled"], warm, cache, 1800)

        with ThreadPoolExecutor(max_workers=args.jobs) as pool:
            results = list(pool.map(lambda path: both(binary, path, cache, args.timeout), inputs.values()))

    out = Path(args.out)
    mismatched = 0
    for name, sides in zip(inputs, results):
        found = differences(sides)
        if found:
            mismatched += 1
            write_mismatch(out, name, sides, found)
            print(f"DIFF {name}: {', '.join(found)}")
    failing = sum(1 for sides in results if sides["interp"]["check"]["status"] != 0)
    print(f"Engines: {len(results) - mismatched}/{len(results)} matched "
          f"({failing} inputs fail to check with the interpreter); mismatches under {out}")
    refusals = [name for name, sides in zip(inputs, results) if refused(sides)]
    for name in refusals:
        print(f"REFUSED {name}")
    print(f"Inputs reporting `{REFUSED}`: {len(refusals)}")
    return 1 if mismatched or refusals else 0


if __name__ == "__main__":
    sys.exit(main())
