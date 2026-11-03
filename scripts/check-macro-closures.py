#!/usr/bin/env python3
"""Check the compile-time code of every macro over the expansion corpus and group what the front end reports.

  check-macro-closures.py [--staple PATH] [--stdlib DIR] [--mode all|1] [--only PATTERN] [--limit N]

Runs `staple check` with `STAPLE_CHECK_MACRO_CLOSURES` set over the inputs
`compare-expansion.py` uses, and prints the diagnostics grouped by message
with the number of inputs and distinct macros that hit each one. The standard
library is part of every check program, so a problem in its compile-time code
shows up for every input; the report counts the distinct source locations too.
"""
import argparse
import importlib.util
from collections import defaultdict
from concurrent.futures import ThreadPoolExecutor
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]

spec = importlib.util.spec_from_file_location("compare_expansion", ROOT / "scripts/compare-expansion.py")
compare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(compare)

DIAGNOSTIC = re.compile(r"^(?:staple: )?(?P<file>[^\n]*?): (?P<message>.*?)(?: at line (?P<line>\d+), column (?P<column>\d+))?$")
NOTE = re.compile(r"^\s+note: while checking the compile-time code of macro `(?P<macro>[^`]+)`")


def check(binary, source, stdlib, mode, timeout):
    environment = {**os.environ, "STAPLE_CHECK_MACRO_CLOSURES": mode}
    try:
        result = subprocess.run([str(binary), "check", "--stdlib", str(stdlib), str(source)],
                                capture_output=True, timeout=timeout, env=environment)
    except subprocess.TimeoutExpired:
        return [("timeout", None, None, None)]
    findings = []
    lines = result.stderr.decode(errors="replace").splitlines()
    for line in lines:
        note = NOTE.match(line)
        if note and findings:
            findings[-1] = findings[-1][:3] + (note["macro"],)
            continue
        found = DIAGNOSTIC.match(line)
        if found and not line.startswith(" "):
            where = f"{Path(found['file']).name}:{found['line']}" if found["line"] else Path(found["file"]).name
            findings.append((found["message"], where, found["file"], None))
    return findings


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--staple", help="staple binary (default: build release)")
    parser.add_argument("--stdlib", default=str(ROOT / "stdlib"))
    parser.add_argument("--mode", default="all", choices=["all", "1"])
    parser.add_argument("--only", help="only inputs whose name matches this regular expression")
    parser.add_argument("--limit", type=int, default=0, help="stop after this many inputs")
    parser.add_argument("--timeout", type=int, default=120)
    args = parser.parse_args()

    binary = Path(args.staple).resolve() if args.staple else compare.build(ROOT)
    with tempfile.TemporaryDirectory(prefix="staple-check-closures-") as directory:
        inputs = compare.collect_inputs(Path(directory))
        names = sorted(name for name in inputs if not args.only or re.search(args.only, name))
        if args.limit:
            names = names[:args.limit]
        with ThreadPoolExecutor(max_workers=os.cpu_count() or 4) as pool:
            results = list(pool.map(lambda name: check(binary, inputs[name], args.stdlib, args.mode, args.timeout), names))

    messages = defaultdict(lambda: {"inputs": set(), "macros": set(), "places": set(), "example": None})
    for name, findings in zip(names, results):
        for message, where, _, macro in findings:
            entry = messages[message]
            entry["inputs"].add(name)
            entry["places"].add(where)
            if macro:
                entry["macros"].add(macro)
            entry["example"] = entry["example"] or (name, where)
    print(f"{len(names)} inputs, {sum(1 for findings in results if findings)} with diagnostics, "
          f"{len(messages)} distinct messages")
    for message, entry in sorted(messages.items(), key=lambda item: -len(item[1]["places"])):
        example = f"{entry['example'][0]} ({entry['example'][1]})"
        print(f"{len(entry['places']):5} places {len(entry['macros']):4} macros {len(entry['inputs']):5} inputs  "
              f"{message}\n        e.g. {example}")
    return 1 if messages else 0


if __name__ == "__main__":
    sys.exit(main())
