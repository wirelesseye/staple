#!/usr/bin/env python3
"""Snapshot and compare `staple expand` output for the standard library, the corpus and the examples.

  capture DIR   run `staple expand` over every input and store the results under DIR
  compare A B   report inputs whose expansion differs between two captured directories

Inputs are the standard library's entry files, every corpus program (written
out by the corpus dump test), the examples and the compiler test fixtures. For
each one the capture stores the exit status, stderr and the rendered module
from `staple expand` (not from the formatter). Hygiene names such as
`True__macro_18` depend on allocation order, so they are renumbered in order
of first appearance within each file.
"""
import argparse
import json
import os
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
import re
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
HYGIENE = re.compile(r"__macro_(\d+)")


def build(root):
    subprocess.run(["cargo", "build", "--release", "--quiet", "-p", "staple"], cwd=root, check=True)
    return root / "target/release/staple"


def normalize(text):
    numbers = {}

    def renumber(match):
        return f"__macro_{numbers.setdefault(match.group(1), len(numbers))}"

    return HYGIENE.sub(renumber, text)


def dump_corpus(dump):
    environment = {**os.environ, "STAPLE_CORPUS_DUMP": str(dump)}
    result = subprocess.run(["cargo", "test", "--quiet", "-p", "staple-compiler", "--lib", "dump_corpus_sources",
                             "--", "--ignored"], cwd=ROOT, capture_output=True, env=environment)
    if result.returncode:
        raise RuntimeError(f"corpus dump failed:\n{result.stderr.decode(errors='replace')}\n"
                           f"{result.stdout.decode(errors='replace')}")
    manifest = [json.loads(line) for line in (dump / "manifest.jsonl").read_text().splitlines()]
    return {f"corpus/{entry['name']}": dump / entry["name"] / "main.sta" for entry in manifest}


def collect_inputs(dump):
    inputs = {}
    for pattern, prefix in [("stdlib/std/**/*.sta", "stdlib"), ("staple-compiler/examples/**/*.sta", "examples"),
                            ("staple-compiler/tests/fixtures/**/*.sta", "fixtures")]:
        for path in sorted(ROOT.glob(pattern)):
            inputs[f"{prefix}/{path.relative_to(ROOT / pattern.split('/**')[0])}"] = path
    inputs.update(dump_corpus(dump))
    return inputs


def expand(binary, source, timeout, stdlib=None):
    try:
        result = subprocess.run([str(binary), "expand", "--stdlib", str(stdlib or ROOT / "stdlib"), str(source)],
                                capture_output=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return dict(status="timeout", stdout="", stderr="")
    return dict(status=result.returncode, stdout=normalize(result.stdout.decode(errors="replace")),
                # Diagnostics name the input's path, which differs between runs.
                stderr=normalize(result.stderr.decode(errors="replace").replace(str(source), "<input>")))


def capture(args):
    binary = Path(args.staple).resolve() if args.staple else build(ROOT)
    destination = Path(args.directory)
    with tempfile.TemporaryDirectory(prefix="staple-expansion-") as directory:
        inputs = collect_inputs(Path(directory))
        if args.stdlib:
            # The standard library's own entry files come from the one being tested.
            inputs = {name: (Path(args.stdlib) / "std" / name[len("stdlib/"):] if name.startswith("stdlib/") else path)
                      for name, path in inputs.items()}
        with ThreadPoolExecutor(max_workers=args.jobs) as pool:
            results = list(pool.map(lambda source: expand(binary, source, args.timeout, args.stdlib), inputs.values()))
    for name, result in zip(inputs, results):
        target = destination / name
        target.parent.mkdir(parents=True, exist_ok=True)
        (target.parent / (target.name + ".expanded")).write_text(result["stdout"])
        (target.parent / (target.name + ".json")).write_text(
            json.dumps(dict(status=result["status"], stderr=result["stderr"]), indent=2) + "\n")
    failed = sum(1 for result in results if result["status"] != 0)
    print(f"Captured {len(results)} inputs under {destination} ({failed} exit non-zero)")


def load(directory):
    records = {}
    for path in sorted(Path(directory).rglob("*.json")):
        name = str(path.relative_to(directory))[:-len(".json")]
        records[name] = (json.loads(path.read_text()), path.with_name(path.name[:-len(".json")] + ".expanded").read_text())
    return records


def compare(args):
    old, new = load(args.old), load(args.new)
    differing = []
    for name in sorted(old.keys() | new.keys()):
        if old.get(name) != new.get(name):
            differing.append(name)
            print(f"DIFF {name}" + ("" if name in old and name in new else " (missing on one side)"))
    print(f"Expansion gate: {len(old.keys() | new.keys()) - len(differing)}/{len(old.keys() | new.keys())} matched")
    return 1 if differing else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    capture_parser = commands.add_parser("capture")
    capture_parser.add_argument("directory")
    capture_parser.add_argument("--staple", help="binary to run; default builds the release binary")
    capture_parser.add_argument("--stdlib", help="standard library to expand against; default is this checkout's")
    capture_parser.add_argument("--jobs", type=int, default=os.cpu_count())
    capture_parser.add_argument("--timeout", type=float, default=120)
    compare_parser = commands.add_parser("compare")
    compare_parser.add_argument("old")
    compare_parser.add_argument("new")
    args = parser.parse_args()
    return capture(args) if args.command == "capture" else compare(args)


if __name__ == "__main__":
    sys.exit(main())
