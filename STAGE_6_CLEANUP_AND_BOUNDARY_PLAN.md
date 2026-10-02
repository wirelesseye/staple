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

### Step 5: Module documentation and language reference (complete)

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

## Execution Notes

### Step 1: Baseline (complete)

- The reference is a release build of `9428582` (this plan's commit). `dump_corpus_sources` wrote 84 corpus programs; with the 11 examples and `game_loop`, that makes 96 paths, and all 96 self-compare as single variants.
- **Baseline counts** over `staple-compiler/src`, `staple-compiler/tests`, `staple-cli/src`, and `scripts`:

  | Category | Count | Command |
  | --- | --- | --- |
  | stage references | 488 | `rg -n "Stage [0-9]"` |
  | plan-file names | 4 | `rg -n "STAGE_\|_PLAN\|BREAKDOWN"` |
  | decision IDs | 62 | `rg -n "\((D[1-6]\|O[1-4]\|K[1-6]\|M[1-4]\|P[1-6]\|R[1-5]\|E[1-9]\|F[1-9])\)\|Contract [1-7]"` |
  | legacy comments | 307 | `rg -n "//.*[Ll]egacy"` |
  | `stage_` test names | 13 | `rg -n "fn stage_[0-9]"` |
  | file-wide dead-code allowances | 3 | in `staple-compiler/src` |
  | item-level dead-code allowances | 41 | in `staple-compiler/src` and `staple-cli/src` |

### Step 2: Unused accessors and dead code (complete)

**Method.** Every dead-code allowance except the justified ones was removed, then each warning was resolved in two passes:

- Items that warned even in the library's test build (`cargo check --lib --profile test`) were dead outright and deleted.
- Items that warned only in the plain library build were used only by tests. Each became `#[cfg(test)]`, or was deleted together with the tests that existed only to exercise it.

**Deleted (production and test code):**

- **`specialization.rs`, superseded designs:**
  - the canonical byte encoding of keys (`SpecializationKey`, the version and family-tag constants, every `encode`/`canonical_encoding`);
  - template-mode key conversion (`ConversionMode`, the `template` constructors, `CanonicalType::Parameter`, `CanonicalEffectSet::variable`, and the `unresolved_*` checks with their `InstanceKeyError` variants);
  - the raw `InstanceRequest` recipe;
  - the never-built `ArtifactSiteOwner::Artifact` and `ArtifactSite::PlanLocal`, and the `Curried`/`ImplicitThunk` adapter kinds.

  Canonical keys are now concrete by construction: the type has no way to hold an unresolved parameter.
- **Never-constructed lowering variants and their match arms:**
  - `ExpressionDisposition::Deferred`;
  - `LoweredExpressionKind::Stage26Deferred`;
  - `DeferredExpressionFamily::{Resource, Coroutine}`;
  - `LoweredCallableAdapter::{Curried, ImplicitThunk}`;
  - `TraitEvidence::RejectedImplementation`;
  - `CompilerHelper` from `LoweredCallableTarget`, `LoweredCallableCategory`, and `CallRoute`, with `request_helper`.
- **Lowering-schema fields nothing reads:**
  - `LoweredName::{storage, captured_cell, moved, move_parameter}`;
  - `LoweredClosureCapture::{drops_value, mutable_storage, derived}`;
  - `LoweredCall::{mutations, moves}`;
  - `LoweredLoop::body_falls_through`, together with `block_falls_through` and `expression_diverges`;
  - `LoweredSymbol::move_parameter`;
  - `LoweredTypeMetadata::{kind, parameters, representation}` and `LoweredTypeKind`;
  - `LoweredTraitMetadata::functional_dependencies`;
  - `LoweredSemanticIds::entry_reactive_required`;
  - `LoweredOwnedBinding::pattern`;
  - `ResolvedInstanceRequest::origin`;
  - the initializer request root's `origin`;
  - `BodyValidator::owner`;
  - a test hook's `origin`.
- **Unused methods:**
  - the emission view's `types`, `type_metadata`, `traits`, `string_formatting`, `instance_evidence`, `initializer_evidence`, and `plan`;
  - `LoweredBoundTarget::artifact_ordinal`;
  - `RelevantParameters::{contains_effect, extend}`;
  - `RuntimeRequirementSet::is_empty`;
  - `SpecializationCatalog::planned_artifact_name`.
- **Checker and resolver:**
  - `TypedModule::state_accesses_of_expression` and the `expression_state_accesses` copy;
  - the `type_representations`/`type_parameter_templates` copies with their accessors;
  - `trait_functional_dependencies`;
  - `ResolvedTypeParameterInfo` and `type_parameters_in_id_order`;
  - `CoroutinePlan::captures`.
- **Six tests** that tested only removed code:
  - template conversion;
  - `InstanceRequest` resolution;
  - the canonical encoding;
  - the deferred-family validator;
  - the `CompilerHelper` rejection;
  - `drops_value` recomputation.

  Tests that used the encoding as a stand-in for key equality now compare keys directly. The finalizer cross-check that used `drops_value` now inlines its rule (an owned capture of a droppable type).

**Test-only (`#[cfg(test)]`):**

- **Methods and constants:**
  - the `ALL` decision-table constants;
  - `CallRoute::category`;
  - `Stage26Route::record_family`;
  - the emission view's `modules`, `concrete_needs_drop`, `instance_captures`, and `instance_parameters`;
  - `resolved_evidence`;
  - `contains_type`, `is_empty`, and `len`;
  - `PARAMETER_RECORD_FAMILIES`;
  - `for_runtime_symbol` and `requirements`;
  - `SpecializationCatalog::planned_names`.
- **Lookup fields**, still filled by lowering under the same attribute:
  - `LoweredTypeMetadata::name`;
  - `LoweredModuleInfo::{qualified_name, companion, executable_entry}`;
  - `LoweredInitializer::executable_entry`;
  - `LoweredInterpolation::format`.

**Kept, with a stated reason:**

- the nine `codegen/layout.rs` runtime-layout constants, each naming the `.ll` runtime that reads it;
- the macro-generated `for_test` arena constructor, which tests use for only some ID types;
- the LSP's `KEYWORD` legend slot, kept for protocol stability.

`TypedModule::syntax` and `state_accesses_of_function` stay public API: the integration tests use them, and those compile as a separate crate, where `#[cfg(test)]` cannot reach.

**Results:**

- **Allowances:** none file-wide, down from 3, and 11 item-level, down from 41, each with a reason.
- **Warnings:** `cargo build --workspace` and `cargo check --workspace --tests` are warning-free.
- **Suite:** 1301 tests pass. That is six fewer than before, because the deleted tests covered only removed code.
- **IR identity (C4):** all 96 paths are `same` against the reference.
- **Size:** 19 files changed, 2120 lines deleted.

### Step 3: Self-contained comments, names, and corpus metadata (complete)

- Rewrote migration-prefixed comments as current rules. Removed obsolete
  guard/placeholder explanations and deleted-function citations, including
  runtime LLVM comments. The largest emitter and lowering files were reviewed
  for ownership order, linkage, captures, coercions, and pattern tests.
- Renamed stage-prefixed tests, helpers, validator hooks, and the extern fixture.
  The compact `stage26` route-table name is now
  `resource_coroutine_route_table_covers_every_route`. The schema families are
  `OrdinaryExpressionFamily` and `ResourceCoroutineRoute`; coroutine scanning
  uses `CoroutineScanVisitor`.
- Replaced `CorpusProgram::substage` with `topic`. Structural coverage still
  selects precisely the former structural entries. Replaced `CorpusD5` and
  `d5` with `CorpusGenericArtifacts` and `generic_artifacts`.
- The comparison script documents normalized, variant-aware compiler comparison
  without referring to a migration plan.
- Rebuilt the reference from `9428582` in `/private/tmp/staple-cleanup-reference`
  and exported all 84 current corpus entries to `/private/tmp/staple-cleanup-corpus`.

**Results:** 1301 workspace tests pass; the workspace build is warning-free.
Formatting and whitespace checks pass. All 96 paths are `same` against
`9428582` (the 12 example paths sampled twice; all 84 dumped corpus entries
compared as single variants). The required mechanical scans, plus compact
stage-prefix and substage scans, are empty.

### Step 4: Diagnostics and executable defect reproduction (complete)

Audited 63 explicit production panic sites, plus two UTF-8 expects in the test-only catalog
ABI helper. Every production site is an internal invariant: classification
establishes symbol kinds, arena/catalog traversal establishes lookup identity,
and codegen operates on declared signatures, validated plans, and blocks it
creates itself. No well-typed source program can violate these preconditions.
54 production sites (and both test-helper name sites) now return diagnostics;
nine non-Result production sites retain named invariant expects.
Functions returning `Result` now return `internal invariant violated: ...`
diagnostics instead of panicking. Non-Result bookkeeping helpers retain
`expect` messages naming their invariant. The seven census/test-only sites
are not production panics and remain test assertions.

The following inventory lists every production site before conversion. Repeated
rows are distinct occurrences, even when their invariant message is identical.

| File / function | Original site | Guarantee | Disposition |
| --- | --- | --- | --- |
| `lower.rs` / `lower_function_body` | lowered body | ID comes from the same arena traversal | internal diagnostic |
| `lower.rs` / `lower_reactive_intrinsic_operation` | the predicate callback was just recorded | callback was inserted immediately before lookup | internal diagnostic |
| `lower.rs` / `lower_expression_occurrence` | rejected above | earlier dispatch rejects this branch | internal diagnostic |
| `lower.rs` / `lower_product` | fallback products are positional | fallback builder appends only positional steps | internal diagnostic |
| `lower.rs` / `lower_designated_product` | designators always have a name | designated syntax carries its name | internal diagnostic |
| `lower.rs` / `lower_callable_value` | constructor values are symbol-selected | route classifiers establish symbol kind | internal diagnostic |
| `lower.rs` / `lower_callable_value` | checked constructor symbol | route classifiers establish symbol kind | internal diagnostic |
| `lower.rs` / `lower_callable_value` | extern values are symbol-selected | route classifiers establish symbol kind | internal diagnostic |
| `lower.rs` / `lower_callable_value` | intrinsic values are symbol-selected | route classifiers establish symbol kind | internal diagnostic |
| `lower.rs` / `lower_callable_value` | checked intrinsic symbol | route classifiers establish symbol kind | internal diagnostic |
| `lower.rs` / `lower_call` | generic direct calls are symbol-selected | route classifiers establish symbol kind | internal diagnostic |
| `lower.rs` / `lower_call` | checked generic function symbol | route classifiers establish symbol kind | internal diagnostic |
| `lower.rs` / `lower_call` | extern calls are symbol-selected | route classifiers establish symbol kind | internal diagnostic |
| `lower.rs` / `lower_call` | intrinsic calls are symbol-selected | route classifiers establish symbol kind | internal diagnostic |
| `lower.rs` / `lower_call` | checked intrinsic symbol | route classifiers establish symbol kind | internal diagnostic |
| `lower.rs` / `lower_call` | constructor calls are symbol-selected | route classifiers establish symbol kind | internal diagnostic |
| `lower.rs` / `lower_call` | checked constructor symbol | route classifiers establish symbol kind | internal diagnostic |
| `lower.rs` / `lower_call` | non-concrete call routes defer above | earlier dispatch rejects this branch | internal diagnostic |
| `lower.rs` / `place_call_arguments` | designated elements always have a name | designated syntax carries its name | internal diagnostic |
| `lower.rs` / `assign_initializer_names` | initializer exists | ID comes from the same arena traversal | named invariant expect |
| `lower/structural_artifacts.rs` / `select_concrete_trait_method_with_kind` | checked above | preceding concrete-evidence / homogeneous-product check | internal diagnostic |
| `lower/structural_artifacts.rs` / `structural_index_body` | checked above | preceding concrete-evidence / homogeneous-product check | internal diagnostic |
| `lower/runtime_requirements.rs` / `position` | every requirement is listed in ALL | exhaustive requirement enum and ALL list | named invariant expect |
| `lower/instance_resolution.rs` / `find_cycle` | a cycle has at least one remaining node | nonempty cycle remainder | named invariant expect |
| `lower/instance_resolution.rs` / `select_method` | matched implementation is in the catalog | matched index comes from the owned implementation catalog | internal diagnostic |
| `lower/coroutine_artifacts.rs` / `plan_original` | artifact {} has a plan while validating | validator cloned this plan from an existing artifact | named invariant expect |
| `lower/artifact_validation.rs` / `validate_closed_catalog` | initializer exists | initializer names enumerate existing initializer IDs | named invariant expect |
| `lower/instance_body.rs` / `enclosing_request` | instance body materialization starts from interned instances | catalog reservation precedes queue insertion and body materialization | named invariant expect |
| `lower/instance_body.rs` / `enclosing_request` | interned instance has a catalog key | catalog reservation precedes queue insertion and body materialization | named invariant expect |
| `lower/worklist.rs` / `resolved_request` | worklist queue holds interned instances | catalog reservation precedes queue insertion and body materialization | named invariant expect |
| `lower/worklist.rs` / `resolved_request` | interned instance has a catalog key | catalog reservation precedes queue insertion and body materialization | named invariant expect |
| `codegen/runtime.rs` / `build_utf8_validator` | fixed runtime parameter exists | helper declares its fixed pointer/length signature | internal diagnostic |
| `codegen/runtime.rs` / `build_utf8_validator` | fixed runtime parameter exists | helper declares its fixed pointer/length signature | internal diagnostic |
| `codegen/ir.rs` / `register_gc_root_region` | GC root registration function | recorded GC runtime is linked | internal diagnostic |
| `codegen/ir.rs` / `begin_sum_storage` | sum payload field | fixed sum/buffer layout | internal diagnostic |
| `codegen/ir.rs` / `extract_sum_alternative` | sum payload field | fixed sum/buffer layout | internal diagnostic |
| `codegen/ir.rs` / `build_string_literal_pattern_compare` | match function | match block was appended to a function | internal diagnostic |
| `codegen/ir.rs` / `trap_if_buffer_capacity_overflows` | Buffer data field has an offset | fixed sum/buffer layout | internal diagnostic |
| `codegen/ir.rs` / `build_buffer_allocation` | Buffer data field has an offset | fixed sum/buffer layout | internal diagnostic |
| `codegen/lowered/mod.rs` / `bind_parameters` | emitted instance has a body | validated instance has a materialized body | internal diagnostic |
| `codegen/lowered/mod.rs` / `emit_main` | GC runtime stack initializer | recorded GC runtime is linked | internal diagnostic |
| `codegen/lowered/mod.rs` / `emit_item` | unit is a basic value | Unit is emitted as an LLVM basic value | internal diagnostic |
| `codegen/lowered/mod.rs` / `emit_item` | break block | emitter creates and positions this control-flow block/context | internal diagnostic |
| `codegen/lowered/mod.rs` / `emit_item` | break loop context | emitter creates and positions this control-flow block/context | internal diagnostic |
| `codegen/lowered/mod.rs` / `emit_product` | product steps always place or spread | validated product steps place or spread | internal diagnostic |
| `codegen/lowered/mod.rs` / `emit_loop` | loop code generation context | emitter creates and positions this control-flow block/context | internal diagnostic |
| `codegen/lowered/mod.rs` / `emit_match` | match arm block | emitter creates and positions this control-flow block/context | internal diagnostic |
| `codegen/lowered/mod.rs` / `emit_match_pattern_branch` | match function | emitter creates and positions this control-flow block/context | internal diagnostic |
| `codegen/lowered/mod.rs` / `emit_match_pattern_branch` | match function | emitter creates and positions this control-flow block/context | internal diagnostic |
| `codegen/lowered/mod.rs` / `emit_match_pattern_branch` | match function | emitter creates and positions this control-flow block/context | internal diagnostic |
| `codegen/lowered/mod.rs` / `emit_match_pattern_branch` | match function | emitter creates and positions this control-flow block/context | internal diagnostic |
| `codegen/lowered/mod.rs` / `emit_logical` | logical right block | emitter creates and positions this control-flow block/context | internal diagnostic |
| `codegen/lowered/mod.rs` / `assemble_call_argument` | unit is basic | Unit is emitted as an LLVM basic value | internal diagnostic |
| `codegen/lowered/mod.rs` / `emit_mutate_index_assignment` | indexed assignment is dispatched for an indexed place | assignment dispatcher matched indexed place | internal diagnostic |
| `codegen/lowered/mod.rs` / `emit_buffer_pop` | Option payload | fixed Option tag/payload layout | internal diagnostic |
| `codegen/ir/coroutines.rs` / `begin_coroutine_resume` | resume frame parameter | declared frame/payload signature | internal diagnostic |
| `codegen/ir/coroutines.rs` / `build_coroutine_cleanup` | cleanup frame parameter | declared frame/payload signature | internal diagnostic |
| `codegen/ir/coroutines.rs` / `build_coroutine_cleanup` | GC root unregistration function | recorded runtime requirement is linked before emission | internal diagnostic |
| `codegen/ir/coroutines.rs` / `build_until_runner` | fixed runtime parameter exists | declared frame/payload signature | internal diagnostic |
| `codegen/ir/coroutines.rs` / `build_reaction_runner` | fixed runtime parameter exists | declared frame/payload signature | internal diagnostic |
| `codegen/ir/coroutines.rs` / `build_derived_runner` | fixed runtime parameter exists | declared frame/payload signature | internal diagnostic |
| `codegen/ir/coroutines.rs` / `build_coroutine_drive` | coroutine driver | recorded runtime requirement is linked before emission | internal diagnostic |
| `codegen/ir/coroutines.rs` / `build_external_await_suspend` | await block | emitter positioned the builder in the await block | internal diagnostic |

The test-only catalog-name sites are also converted to internal diagnostics:

| File / function | Original site | Guarantee | Disposition |
| --- | --- | --- | --- |
| `codegen/lowered/mod.rs` / `declared_catalog_types` | planned names are UTF-8 | planned names originate as Rust UTF-8 strings | internal diagnostic |
| `codegen/lowered/mod.rs` / `declared_catalog_types` | initializer names are UTF-8 | planned names originate as Rust UTF-8 strings | internal diagnostic |

`cstring_temporary_through_borrowed_callback_leaks` is an ignored CLI test that
compiles, links, and runs the known leaking program twice. It calls a borrowed
non-extern callback that delegates to libc `strlen`. Its comment specifies the
allocation/release accounting a fixed test must assert. A successful run alone
does not prove the leak is fixed; the defect remains documented in `lower_call`
and `LoweredCall::c_string_temporary`.

**Results:** 1301 workspace tests pass (two intentionally ignored tests). The
ignored leak reproduction also passes when selected explicitly, including the
asserted `strlen` results from both invocations. The workspace and test builds
are warning-free. All 96 reference comparisons are `same`. Formatting and
whitespace checks pass. Corpus prose was reviewed again to remove descriptions
of removed-backend failures that could otherwise read as current behavior.

### Step 5: Module documentation and language reference (complete)

- Crate rustdoc explains parse → expand → resolve → check → lower → emit,
  each output and its consumer, and the distinction between compiler object
  emission and CLI executable linking. Codegen reads only the owned lowered
  program and its emission view.
- Resolver, checker, lowering, and codegen docs describe responsibilities and
  guarantees. Lowering docs cover owner-relative arenas, canonical instances,
  fixed-point catalog closure, validators, recorded runtime requirements, and
  the immutable emission boundary. Codegen docs cover planned names/linkage,
  inline cleanup, recorded semantic choices, target layout, and concrete closure,
  resource, mutation, coroutine, sum/cell, and native extern ABI conventions.
- Main lowering and codegen submodules have standalone overviews, including the
  test-only census. Remaining placeholder/future-tense prose was rewritten as
  current construction, expansion, and validation rules. Comments referring to
  removed `InstanceRequest`/`CompilerHelper` designs are gone. The key-family
  test now describes unique names and structural keys rather than deleted byte
  encodings.
- `Staple.md` states normal and early task-scope closure, and product-field signal
  notification. Its generic `Drop` selection/bound restriction and declared
  coroutine-row upper-bound rules were checked and retained.
- README identifies the syntax crate and typed lowering and points to crate docs.
- Rustdoc, including private modules, builds with warnings denied. A public link
  to the private `LoweredProgram` type was changed to code text so ordinary public
  documentation remains valid too.

**Results:** All 1301 workspace tests pass (two intentionally ignored tests).
Workspace and test builds are warning-free, and rustdoc including private modules
builds with warnings denied. All 36 example CLI paths pass. Formatting and
whitespace checks pass. The final reference IR results are recorded in Step 6.
