#!/usr/bin/env python3
"""Compare stdout and exit status for the runnable corpus and standalone examples.

Accept existing --old/--new binaries, or build --old-ref in a temporary checkout
and the current release binary. --capture writes reference results as JSON;
--reference reuses them and runs only the new compiler.
Use --old-source-root with --old to compare a syntax migration against the
baseline checkout's corpus, examples, and standard library. Both checkouts
must contain exactly the same runnable program identities.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def build(root):
    subprocess.run(["cargo", "build", "--release", "--quiet"], cwd=root, check=True)
    return root / "target/release/staple"


def collect_programs(root, dump, target_dir=None):
    # The corpus dump writes each program to <dump>/<name>/main.sta (with a
    # file entry's sibling modules) and lists them in manifest.jsonl.
    environment = {**os.environ, "STAPLE_CORPUS_DUMP": str(dump)}
    if target_dir is not None:
        environment["CARGO_TARGET_DIR"] = str(target_dir)
    result = subprocess.run(["cargo", "test", "--quiet", "-p", "staple-compiler", "--lib", "dump_corpus_sources",
                             "--", "--ignored"], cwd=root, capture_output=True,
                            env=environment)
    if result.returncode:
        raise RuntimeError(f"corpus dump failed for {root}:\n{result.stderr.decode(errors='replace')}\n"
                           f"{result.stdout.decode(errors='replace')}")
    manifest = [json.loads(line) for line in (dump / "manifest.jsonl").read_text().splitlines()]
    programs = [dict(name=entry["name"], source=dump / entry["name"] / "main.sta")
                for entry in manifest if entry["runnable"]]
    files = {entry["file"] for entry in manifest if entry["file"] is not None}
    for path in sorted((root / "staple-compiler/examples").glob("*.sta")):
        if str(path.relative_to(root)) not in files:
            programs.append(dict(name=path.stem, source=path))
    indexed = {program["name"]: program for program in programs}
    if len(indexed) != len(programs):
        raise RuntimeError(f"{root}: duplicate program identities")
    return indexed


def compare(args, temporary):
    old = Path(args.old).resolve() if args.old else None
    old_root = Path(args.old_source_root).resolve() if args.old_source_root else ROOT
    reference = json.loads(Path(args.reference).read_text()) if args.reference else None
    if old is None and reference is None:
        checkout = temporary / "reference"
        checkout.mkdir()
        archive = subprocess.run(["git", "archive", args.old_ref], cwd=ROOT, check=True, capture_output=True)
        subprocess.run(["tar", "-x", "-C", str(checkout)], input=archive.stdout, check=True)
        old = build(checkout)
        old_root = checkout
    new = Path(args.new).resolve() if args.new else build(ROOT)
    programs = collect_programs(ROOT, temporary / "corpus")
    old_programs = programs
    if reference is None and old_root != ROOT:
        old_target = Path(args.old_target_dir).resolve() if args.old_target_dir else old_root / "target"
        old_programs = collect_programs(old_root, temporary / "old-corpus", old_target)
        if set(old_programs) != set(programs):
            raise RuntimeError("baseline sources do not cover exactly the current runnable corpus and examples")
    results = {}
    failures = []
    for index, program in enumerate(programs.values(), 1):
        source = program["source"]
        outputs = [reference[program["name"]]] if reference is not None else []
        runs = [(new, ROOT / "stdlib", source)]
        if reference is None:
            runs.insert(0, (old, old_root / "stdlib", old_programs[program["name"]]["source"]))
        for binary, stdlib, source in runs:
            result = subprocess.run([str(binary), "run", "--stdlib", str(stdlib), str(source)],
                                    capture_output=True, timeout=args.timeout)
            outputs.append(dict(status=result.returncode, stdout=result.stdout.decode("utf-8")))
            # CLI compile/link errors also return 1; reject them even when both sides fail.
            if result.returncode == 1 and b"staple:" in result.stderr:
                raise RuntimeError(f"{program['name']}: {result.stderr.decode()}")
        results[program["name"]] = outputs[0]
        if outputs[0] != outputs[1]:
            failures.append(program["name"])
            print(f"DIFF {program['name']}: {outputs!r}")
        if index % 20 == 0:
            print(f"Compared {index}/{len(programs)} programs", flush=True)
    if reference is not None and set(reference) != set(results):
        raise RuntimeError("captured reference does not cover exactly the current runnable corpus and examples")
    if args.capture:
        Path(args.capture).write_text(json.dumps(results, indent=2, sort_keys=True) + "\n")
    print(f"Behavior gate: {len(programs) - len(failures)}/{len(programs)} matched")
    return bool(failures)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    baseline = parser.add_mutually_exclusive_group(required=True)
    baseline.add_argument("--old")
    baseline.add_argument("--old-ref")
    baseline.add_argument("--reference", help="captured baseline JSON; run only the new binary")
    parser.add_argument("--new")
    parser.add_argument("--old-source-root", help="baseline checkout for corpus, examples, and stdlib when using --old")
    parser.add_argument("--old-target-dir", help="isolated Cargo target directory for the baseline corpus dump")
    parser.add_argument("--capture")
    parser.add_argument("--timeout", type=float, default=60)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="staple-behavior-") as directory:
        return compare(args, Path(directory))


if __name__ == "__main__":
    raise SystemExit(main())
