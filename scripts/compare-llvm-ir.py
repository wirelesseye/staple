#!/usr/bin/env python3
"""Compare the LLVM IR two `staple` binaries emit for the same programs.

Used for the Stage 5 behavior-preservation gates (see
STAGE_5_LLVM_MIGRATION_BREAKDOWN.md): a refactor or a new emitter must produce
the same normalized IR as the baseline binary.

Normalization makes the comparison independent of definition order:

- top-level entities (each `define` body, and every other non-comment line)
  are sorted;
- within a function, `__staple_gc_register_root` calls are sorted, because the
  legacy `main` harness registers global roots in `HashMap` order.

The legacy backend is also nondeterministic in coroutine frame field
assignment, so one program can normalize to several variants. Each binary
therefore compiles each program `--runs` times, and the comparison is between
the *sets* of variants. Sampling can miss a rare variant, so sets that differ
but share a variant are reported as `OVERLAP`, and disjoint sets where either
side produced more than one variant (or `--runs` is 1) as `INCONCLUSIVE`; both
mean "re-run with more `--runs`". Only disjoint sets of one stable variant
each are a definite `DIFF`.

`--new-subset` checks the Stage 5.8 determinism transition: the new binary
must emit one variant and that variant must occur in the old binary's set.
Later shared-helper gates use the default equal-set comparison.

Example:

    scripts/compare-llvm-ir.py --old /tmp/base/staple --new target/debug/staple \\
        --stdlib stdlib --runs 4 staple-compiler/examples/*.sta

Exit status is 1 when any program differs or fails to compile on one side
only, and 0 otherwise.
"""

import argparse
import difflib
import hashlib
import subprocess
import sys

ROOT_CALL = "@__staple_gc_register_root(ptr"


def normalize(text: str) -> str:
    entities = []
    current = []
    for line in text.split("\n"):
        if current:
            current.append(line)
            if line == "}":
                entities.append(current)
                current = []
        elif line.startswith("define"):
            if line.endswith("}"):
                entities.append([line])
            else:
                current = [line]
        elif line.strip() and not line.startswith(";"):
            entities.append([line])
    rendered = []
    for lines in entities:
        roots = sorted(line for line in lines if ROOT_CALL in line and "call" in line)
        rest = [line for line in lines if not (ROOT_CALL in line and "call" in line)]
        rendered.append("\n".join(rest + roots))
    return "\n\n".join(sorted(rendered)) + "\n"


def emit(binary: str, stdlib: str, program: str):
    result = subprocess.run(
        [binary, "compile", "--stdlib", stdlib, "--emit", "llvm", program],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        return None
    return normalize(result.stdout)


def variants(binary: str, stdlib: str, program: str, runs: int):
    found = {}
    for _ in range(runs):
        ir = emit(binary, stdlib, program)
        if ir is None:
            return None
        found.setdefault(hashlib.sha256(ir.encode()).hexdigest()[:12], ir)
    return found


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--old", required=True, help="baseline staple binary")
    parser.add_argument("--new", required=True, help="staple binary under test")
    parser.add_argument("--stdlib", required=True, help="standard library root")
    parser.add_argument("--runs", type=int, default=4, help="compilations per binary")
    parser.add_argument(
        "--show-diff", action="store_true", help="print a diff for each differing program"
    )
    parser.add_argument(
        "--new-subset", action="store_true",
        help="accept one deterministic new variant contained in the old variants",
    )
    parser.add_argument("programs", nargs="+", help="source files to compile")
    arguments = parser.parse_args()

    failed = False
    for program in arguments.programs:
        old = variants(arguments.old, arguments.stdlib, program, arguments.runs)
        new = variants(arguments.new, arguments.stdlib, program, arguments.runs)
        if old is None and new is None:
            print(f"{program}: does not compile with either binary")
            continue
        if old is None or new is None:
            side = "old" if old is None else "new"
            print(f"{program}: FAILS to compile with the {side} binary only")
            failed = True
            continue
        old_keys, new_keys = set(old), set(new)
        if arguments.new_subset and len(new_keys) == 1 and new_keys <= old_keys:
            print(f"{program}: deterministic subset (old {len(old_keys)} variant(s), new 1)")
            continue
        if old_keys == new_keys:
            print(f"{program}: same ({len(old_keys)} variant(s))")
            continue
        if old_keys & new_keys:
            print(
                f"{program}: OVERLAP (old {sorted(old_keys)}, new {sorted(new_keys)}); "
                "re-run with more --runs"
            )
            failed = True
            continue
        if len(old_keys) > 1 or len(new_keys) > 1 or arguments.runs < 2:
            # A nondeterministic program (or a single run) can sample disjoint
            # subsets of the same variant set, as `coroutines.sta` does at 3
            # runs out of its 4 variants. Only a disagreement between two
            # stable single variants is a definite difference.
            print(
                f"{program}: INCONCLUSIVE (old {sorted(old_keys)}, new {sorted(new_keys)}); "
                "the program is nondeterministic or was compiled once, "
                "re-run with more --runs"
            )
            failed = True
            continue
        print(f"{program}: DIFF (old {sorted(old_keys)}, new {sorted(new_keys)})")
        failed = True
        if arguments.show_diff:
            before = old[sorted(old_keys - new_keys or old_keys)[0]]
            after = new[sorted(new_keys - old_keys or new_keys)[0]]
            sys.stdout.writelines(
                difflib.unified_diff(
                    before.splitlines(keepends=True),
                    after.splitlines(keepends=True),
                    fromfile=f"old/{program}",
                    tofile=f"new/{program}",
                )
            )
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
