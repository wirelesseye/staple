# Stage 6 Plan: Remove Transitional Code and Document the Boundary

This is the plan for Stage 6 of [TYPED_LOWERING_PLAN.md](TYPED_LOWERING_PLAN.md), the last stage of the typed-lowering refactor. Stages 1–5 are complete. There is one emitter, it reads only `LoweredProgram`, and the suite passes 1307 tests.

**Overriding constraint.** All plan and breakdown files (`TYPED_LOWERING_PLAN.md`, every `STAGE_*.md`, including this one) are deleted after the refactor. Nothing that must survive may live only in them. Stage 6 ends with code, tests, rustdoc, `Staple.md`, and the README that are complete and correct on their own, with no reference to a plan file, a stage number, or a migration decision ID.

Line references are against the commit that adds this plan and will drift; re-locate code by name. Build with `CARGO_INCREMENTAL=0`.

## Starting Point

Measured on the current tree:

| Area | Size |
| --- | --- |
| Code comments citing a stage or step (`Stage 5.6 Step 3`) | about 500, led by `lower.rs` (102), `codegen/lowered/mod.rs` (52), `instance_body.rs`, and `codegen/ir.rs` (37 each) |
| Comments explaining code by a deleted legacy function (`Legacy compile_place_pointer: …`) | about 300, 150 of them in `codegen/lowered/mod.rs` |
| Migration decision IDs in comments (`D5`, `Contract 1`, `(O3)`, `(K4)`, `(F7)`, …) | 62 |
| Plan-file names in code | `scripts/compare-llvm-ir.py`'s header; the `STAGE_4_6_EXTERN_FIXTURE` constant in `graph_validation.rs` |
| Test functions named after stages (`stage_4_6_…`, `stage_5_3_…`) | 13 |
| Corpus entries tagged by substage (`CorpusProgram::substage`, `"5.11"`) | 83 tags |
| Dead-code allowances | `#![allow(dead_code)]` on all of `lower.rs` (removing it shows 38 warnings: fields never read, unused variants, methods, and constants); `#![allow(dead_code)]` on all of `specialization.rs` ("Stage 3.1 … before Stage 3.3 uses them"); seven in `resolve.rs` ("Consumed by lowering beginning in Stage 2.3"); two in `coroutine_lower.rs`; about 50 narrower allowances in total |
| `TypedModule` methods without production callers | `state_accesses_of_expression` (no callers); `syntax`, `state_accesses_of_function`, `is_io_type`, `is_task_type` (tests only). Every other method of the 77 has a lowering, frontend, LSP, or checker-internal caller. |
| Production panic sites (`expect`/`unwrap`/`panic!`/`unreachable!`) in `lower*` and `codegen/` | about 66 (`lower.rs` 20, `codegen/lowered/mod.rs` 18, `codegen/ir/coroutines.rs` 8, `codegen/ir.rs` 6, the rest one or two each) |
| Module documentation | only `lower.rs` has a module doc, and it is three lines; `lib.rs`, `typecheck.rs`, `resolve.rs`, `codegen/mod.rs`, and the lowering/codegen submodules have none or a one-liner |

Knowledge that currently lives only in plan files and must move:

- **The one remaining known defect.** A temporary `CString` passed through a non-extern callable leaks. It is now fully described at its site in `lower_call` (cause, why the fix is not trivial, fix direction, reproduction) and on `LoweredCall::c_string_temporary`, so the plan entry can go. Stage 6 adds an ignored reproduction test, so the defect is executable, not just prose.
- **Language rules changed during the refactor.** These are generic `Drop` selection and its bound restriction, declared coroutine rows as upper bounds, signal field-write notification, and task scopes closing on early exits. All are in `Staple.md` except the task-scope early-exit rule, which Step 5 adds.
- **The pipeline's phase responsibilities and invariants.** These exist only across the plan files. Step 5 writes them as rustdoc.
- **The IR-comparison method** (`scripts/compare-llvm-ir.py`) stays as a tool. Its header explains its purpose without citing the breakdown.

## Decisions Specific to Stage 6

**C1: Comments state the rule, not its history.** A comment explains what the code does and why, in terms of the current program. Stage numbers, step numbers, decision IDs, plan names, and "legacy X did …" justifications are removed or rewritten.

- **Legacy references.** A comment that justified behavior by citing a deleted legacy function keeps the substance and drops the citation. For example, "Legacy `compile_place_pointer` returns the root symbol" becomes "the place's root symbol: the symbol or captured cell at its base".
- **Genuine history.** Where history explains a surprising choice (a mirrored rule, a compatibility constraint), the comment says so in plain words.
- **Comment density** stays like the surrounding code. This is a rewrite, not an expansion.

**C2: Every dead-code allowance is either removed or justified at the item.**

- **Delete** the blanket `#![allow(dead_code)]` in `lower.rs` and `specialization.rs`, and the stale per-item ones in `resolve.rs` and `coroutine_lower.rs`.
- **Each resulting warning** is resolved by:
  - deleting the item;
  - `#[cfg(test)]`, when only tests use it;
  - or a narrow `#[allow(dead_code)]` with a comment naming the concrete reason it must stay (for example a variant that is constructed only by a validator's corruption test, or a record field kept for diagnostics).
- **Lowering-schema fields** that nothing reads are deleted, not hidden. A field read only by a validator is in use, not dead.

**C3: Panics in lowering and codegen follow one rule.** Lowering's input is a type-checked program, and codegen's input is a validated `LoweredProgram`. Each site is one of two kinds:

- **Internal invariant:** guaranteed by the checker or a validator. It may panic, through `expect` with a message naming the invariant. Where the function already returns `Result`, it reports the codebase's existing "internal invariant violated: …" diagnostic instead, matching the 5.9 emitter convention.
- **User-reachable:** a well-typed program can trigger it. It becomes a source-located diagnostic, with a test proving that.

Each site's classification is recorded in the Step 4 notes. If one is unclear, write a program that tries to reach it.

**C4: Stage 6 changes no emitted IR.** Every step is checked against a release reference built at the commit that adds this plan. `scripts/compare-llvm-ir.py` must report `same` over the examples, `game_loop`, and every corpus program, dumped with `dump_corpus_sources`. Converting an unreachable panic to a diagnostic cannot change IR. A change that does is a bug in the step.

**C5: The ABI statement is evidence, not a new test.** The "concrete ABIs unchanged" requirement was proven along the way:

- Stage 5.9's shadow comparison of 589 programs, with identical LLVM function types for every mapped function;
- Stage 5.10's R1 identity across the cutover;
- Stage 5.11's per-fix containment;
- C4 here.

Stage 6 records the result in the codegen module documentation (what the ABI is), not as history.

## Steps

Each step ends with `cargo nextest run --workspace`, the C4 comparison, a warning-free `cargo build --workspace`, formatting, `git diff --check`, and a commit.

### Step 1: Baseline

- Build the release reference at this plan's commit, dump the corpus, and confirm that all paths self-compare as single variants.
- Record the counts from the Starting Point table with the exact `rg` commands, so Step 6 can show them at zero.

### Step 2: Unused accessors and dead code (C2)

- **Unused accessors.** Delete `TypedModule::state_accesses_of_expression`. Move `syntax`, `state_accesses_of_function`, `is_io_type`, and `is_task_type` behind `#[cfg(test)]`, or delete them if their tests can use a lowering-side query instead.
- **Dead-code allowances.** Remove the blanket allowances and resolve every warning under C2. List the deleted schema items in the step notes, grouped by owner type.
- **Other warnings.** Run `cargo build --workspace` and `cargo test --no-run` with the allowances gone, and resolve test-only dead code the same way.

### Step 3: Self-contained comments, names, and corpus metadata (C1)

- **Comments.** Rewrite the roughly 500 stage references, 300 legacy citations, and 62 decision IDs, file by file, largest first (`codegen/lowered/mod.rs`, `lower.rs`, `codegen/ir.rs`, `instance_body.rs`, …). Run C4 after each file group; comment changes cannot alter IR, so a `DIFF` means an accidental code edit.
- **Test names.** Rename the 13 `stage_…` tests and the `STAGE_4_6_EXTERN_FIXTURE` constant after what they test.
- **Corpus metadata.** Replace `CorpusProgram::substage` with a topic label (for example `"ownership"`, `"coroutines"`, `"structural"`), or remove the field if nothing reads it. Rename `codegen_corpus` comments and the harness messages that mention substages.
- **Script.** Reword `scripts/compare-llvm-ir.py`'s header to state its purpose (comparing two compiler binaries' LLVM IR, with variant handling), without the breakdown reference.

### Step 4: Diagnostics, not panics (C3)

- Classify the roughly 66 production panic sites in `lower*` and `codegen/`. For an internal invariant, keep a panic or internal diagnostic with a message naming the invariant. For a user-reachable site, add a source diagnostic and a test.
- Add the ignored reproduction test for the `CString` leak, so the remaining known defect is executable. Without allocator instrumentation it can at least compile and run the leaking program, and the comment explains what a fixed version must assert.
- In the step notes, record each classification and every site converted.

### Step 5: Module documentation and language reference

- **Crate overview** (`lib.rs` `//!`): the pipeline order (parse → expand → resolve → check → lower → emit), what each phase consumes and produces, and the rule that each phase reads only its predecessor's output. In particular, codegen reads only `LoweredProgram`, never the checker.
- **Phase docs** for `resolve.rs`, `typecheck.rs`, `lower.rs` (expanded), and `codegen/mod.rs`, covering responsibilities, invariants, and what each guarantees its consumer:
  - lowering: arenas, the closed specialization catalog, concrete instances, validated artifact plans, recorded runtime requirements;
  - codegen: planned names, inline drop glue, no type-based decisions, the ABI conventions.
- **Short module docs** for the main lowering and codegen submodules that lack one: `instance_body`, `worklist`, `artifact_closure`, `cleanup_artifacts`, `coroutine_artifacts`, `census`, `lowered/{coroutines,reactive,structural}`, and `ir/coroutines`.
- **`Staple.md`.** Add the task-scope early-exit rule (`return`/`break`/`continue` close task scopes opened since their target, as they dispose reactive scopes), and check the other refactor-era rules are present.
- **README.** If it describes the compiler's architecture, point it to the crate docs.

### Step 6: Stage 6 gate

- `cargo nextest run --workspace` passes, and `cargo build --workspace` and the test build are warning-free.
- **C4:** every path is `same` against the Step 1 reference.
- **Mechanical checks over `staple-compiler/`, `staple-cli/`, and `scripts/` are empty:**
  - stage references: `rg -n "Stage [0-9]"`;
  - plan-file names: `rg -n "STAGE_|_PLAN|BREAKDOWN"`;
  - decision IDs: `rg -n "\((D[1-6]|O[1-4]|K[1-6]|M[1-4]|P[1-6]|R[1-5]|E[1-9]|F[1-9])\)|Contract [1-7]"`;
  - legacy citations: `rg -n "//.*[Ll]egacy"`, allowing only comments where "legacy" describes something that still exists, each reviewed.
- **No blanket `#![allow(dead_code)]`** remains in `staple-compiler/src`, and every remaining `#[allow(dead_code)]` carries a reason.
- **CLI.** The `--emit llvm`, `--emit object`, and `run` paths pass on the examples with the worktree standard library.

### Step 7: Make the plan files deletable

- Show that nothing outside the plan files references them: run `rg` for each plan file name across the repository, excluding the plans themselves.
- **Close out `TYPED_LOWERING_PLAN.md`** by marking Stage 6 and the refactor complete. Its "Known defects" entry duplicates the in-code description and can go with the file.
- **Leave the deletion of the plan files to you.** This step only proves they are safe to delete, and lists them.

## Ordering

```text
Step 1 → Step 2 → Step 3 → Step 4 → Step 5 → Step 6 → Step 7
```

- **Step 2 before Step 3**, so comments are not rewritten for code that is about to be deleted.
- **Step 3 before Step 5**, so the module docs are written against comments that already state rules rather than history.
- **Step 4 is independent** of Step 3 but touches the same files. Doing it after Step 3 avoids rewriting the same comments twice.
- **Size.** Step 3 is the largest, about 870 comment sites, but it is mechanical and C4 guards it completely. Steps 2 and 4 need judgment per item: 38 dead-code warnings and about 66 panic sites. Two or three implementation turns are realistic, splitting after Step 2 and after Step 4, with a review between.
