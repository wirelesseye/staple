#!/usr/bin/env python3
"""Compare stdout and exit status for the runnable corpus and standalone examples.

Accept existing --old/--new binaries, or build --old-ref in a temporary checkout
and the current release binary. --capture writes reference results as JSON;
--reference reuses them and runs only the new compiler.
"""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def build(root):
    subprocess.run(["cargo", "build", "--release", "--quiet"], cwd=root, check=True)
    return root / "target/release/staple"


def compare(args, temporary):
    old = Path(args.old).resolve() if args.old else None
    reference = json.loads(Path(args.reference).read_text()) if args.reference else None
    if old is None and reference is None:
        checkout = temporary / "reference"
        checkout.mkdir()
        archive = subprocess.run(["git", "archive", args.old_ref], cwd=ROOT, check=True, capture_output=True)
        subprocess.run(["tar", "-x", "-C", str(checkout)], input=archive.stdout, check=True)
        old = build(checkout)
    new = Path(args.new).resolve() if args.new else build(ROOT)
    corpus = subprocess.run(["cargo", "run", "--release", "--quiet", "-p", "staple-compiler", "--example", "export_corpus"],
                            cwd=ROOT, check=True, capture_output=True, text=True)
    programs = [json.loads(line) for line in corpus.stdout.splitlines()]
    files = {p["source"] for p in programs if p["kind"] == "file"}
    for path in sorted((ROOT / "staple-compiler/examples").glob("*.sta")):
        relative = str(path.relative_to(ROOT))
        if relative not in files:
            programs.append(dict(name=path.stem, kind="file", source=relative))
    results = {}
    failures = []
    for index, program in enumerate(programs, 1):
        if program["kind"] == "file":
            source = ROOT / program["source"]
        else:
            source = temporary / (program["name"] + ".sta")
            source.write_text(program["source"])
        outputs = [reference[program["name"]]] if reference is not None else []
        for binary in ((new,) if reference is not None else (old, new)):
            result = subprocess.run([str(binary), "run", "--stdlib", str(ROOT / "stdlib"), str(source)],
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
    parser.add_argument("--capture")
    parser.add_argument("--timeout", type=float, default=60)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="staple-behavior-") as directory:
        return compare(args, Path(directory))


if __name__ == "__main__":
    raise SystemExit(main())
