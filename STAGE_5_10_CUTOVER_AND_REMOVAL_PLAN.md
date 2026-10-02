# Stage 5.10 Plan: Cutover and Removal

This is the plan for the Stage 5.10 section of [STAGE_5_LLVM_MIGRATION_BREAKDOWN.md](STAGE_5_LLVM_MIGRATION_BREAKDOWN.md). First read:

- the breakdown's Migration Contract and Decisions D1–D6;
- the Stage 5.9 plan's handoff ([STAGE_5_9_FULL_SUITE_PARITY_PLAN.md](STAGE_5_9_FULL_SUITE_PARITY_PLAN.md), "Handoff to Stage 5.10" and "Post-gate review fixes").

This plan's gate (Step 8) supersedes the **Gate** paragraph in the breakdown's 5.10 section.

Line references are against `1501270` and will drift; re-locate code by name. Build with `CARGO_INCREMENTAL=0` to bound disk use. Migration-era feature builds are historical; only the default emitter exists now.

## Execution Notes

### Step 1 — complete

- Pre-cutover production reference: `ee50922`, built with `CARGO_INCREMENTAL=0 cargo build --features staple/lowered-emitter --target-dir /private/tmp/staple-stage-5-10-reference`. Binary: `/private/tmp/staple-stage-5-10-reference/debug/staple`. The only source addition during this build is the test-only dump below.
- Added ignored `codegen::differential::tests::dump_corpus_sources`; invoke with `STAPLE_CORPUS_DUMP=/private/tmp/staple-stage-5-10-corpus cargo test -p staple-compiler dump_corpus_sources -- --ignored --nocapture`. It dumped all 74 entries and copies `.sta` companion modules for file entries.
- The final default, lowered, and shadow gates each pass all 1318 tests (one ignored dump helper skipped). Logs: `/private/tmp/staple-5-10-{default,lowered,shadow}.log`. Final legacy ledger: `/private/tmp/staple-5-10-shadow.ledger`, **589 programs, 166552 bodies, 8 D5-explained rejections**.
- Frozen reference for all later R1 gates: `/private/tmp/staple-stage-5-10-reference/release/staple`, built with `CARGO_INCREMENTAL=0 cargo build --release --features staple/lowered-emitter --target-dir /private/tmp/staple-stage-5-10-reference` from the same `ee50922` production source. The supplemental debug sweep also finished with 85 single-variant successes and the original macro-example rejection; the optimized reference makes repeated full-scope gates practical.
- Optimized reference self-comparison passes for **all 86 paths**, four runs per side, one variant each. The initial log `/private/tmp/staple-5-10-reference-release-self.log` contains 85 successes plus the pre-existing rejection of `examples/macros.sta`. That example used parenthesized effectful macro arguments which lowering rejected as positional products. Replacing the two invocations with block arguments makes the unchanged reference compile and run the complete intended tour. `/private/tmp/staple-5-10-reference-macros-self.log` proves its single stable variant; no path remains unverified. Compiler behavior is untouched. The underlying parenthesized-macro lowering rejection remains for a later front-end fix.
- The final test-only split into doc-hidden `dump_corpus` plus ignored `dump_corpus_sources` passes its focused ignored test; the 225 dumped `.sta` files are byte-identical to the original export. The macro example prints all eight intended lines, including `generated values are correct`. Its dependent LSP hover check passes; formatting and diff checks pass. Step 2 is next.

### Step 2 — complete

The source inventory is broader than the initial 31-test list. Also audit `stage_4_5_coroutine_and_runner_transition_matches_legacy_emission`, `legacy_constructor_and_structural_bodies_match_artifact_plans`, `legacy_specializations_not_in_the_catalog_are_detected`, and `stage_4_7_census_accounts_for_every_emitted_function`. The verdicts below are recorded before deletion. Their transition helpers and `instance_for_legacy_specialization` will be retired in dependency order. `coroutine_and_reactive_thunks_bind_demand_driven` and `stage_4_6_unused_subsystems_record_no_requirement` already assert lowering facts without calling legacy; preserve their assertions.

The default now always selects lowered emission. The converted runtime tests use actual LLVM use lists and catalog names for every emitted instance and initializer, with program-wide exact equality and owner-local coverage (owner derivation conservatively includes subsystem needs such as an inline `coro` creation). The exact predeclared-cell owner checks remain. This stronger coverage exposed missing GC requirements for `BufferGet` and `BufferFreeze` interior-pointer registration; lowering now records them. No emission instruction changes: R1 remains the gate. The converted tests pass and 16 legacy-only tests plus their dedicated helpers are retired. Default, lowered, and shadow suites each pass 1302 tests (one ignored dump helper skipped); all 86 R1 paths report `same (1 variant(s))`. Logs: `/private/tmp/staple-5-10-step2-{default,lowered,shadow,ir}.log`.

### Step 3 — complete

The shared 74-entry `codegen/corpus.rs` now exposes `Corpus*` types and `codegen_corpus`. Expectations are `MustRun`/`CompileOnly`; D5 metadata survives. The strict in-process harness retains catalog census, focus definitions, structural coverage and distinct D5 artifacts. The CLI harness retains object/link/run, pinned stdout and traps, and explicitly asserts successful exit codes. The body comparator, normalization test, shadow counters/ledger writers, shadow compilation and `differential-shadow` feature are removed. Default and remaining lowered-feature suites each pass 1301 tests (one ignored dump helper skipped). All 86 four-run R1 paths are `same (1 variant(s))`; formatting, workspace check and diff checks pass. Logs: `/private/tmp/staple-5-10-step3-{default,lowered,ir}.log`. Step 4 is next.

### Step 4 — complete

The legacy declaration census, alias/fallback mapping and specialization matcher are gone. `assert_catalog_census` and the concrete-type helper used by lowering assertions remain. The recorder module, legacy snapshot functions and every test instrumentation block in `ModuleEmitter` are removed; actual LLVM runtime use-list checks remain on lowered emission. The workspace check is warning-free; both remaining suites pass 1301 tests (one ignored helper skipped), all 86 four-run R1 paths match, and formatting/diff checks pass. Logs: `/private/tmp/staple-5-10-step4-{default,lowered,ir}.log`. The IR sweep invokes the unchanged comparison script on four disjoint chunks and verifies exactly 86 matching rows. Step 5 is next.

### Step 5 — complete

`ModuleEmitter`, its AST/checked-program emission state and helper functions, `Emitter`, `with_emitter`, and both `lowered-emitter` features are deleted. `CodeGenerator` directly invokes the only emitter. Four newly unused shared helpers (`add_finalizer_function`, `build_fn_type`, `coroutine_resource_bundle_type`, `variadic_argument_count_matches`) and unused imports are deleted. The `lowered/` directory remains: it groups emission away from the backend helper layers, and a rename would add no useful boundary change. No warning is suppressed. The only new warning is the checker query `drop_method_for`, retained for the explicitly deferred Stage 6 inventory because tests still call it. Codegen itself has no warnings. The single default suite passes 1301 tests (one ignored helper skipped); all 86 four-run R1 paths match; workspace check, formatting and diff checks pass. Logs: `/private/tmp/staple-5-10-step5-{default,ir}.log`. Step 6 is next.

### Step 6 — complete

Lowering's output no longer carries or clones `TypedModule`; its private bridge accessor is gone. The layout agreement test locally retains its checked input and passes. Historical codegen comments are reworded, and the mechanical forbidden-symbol/AST-import check is clean. Debug formatting remains only in test-side D5 owner diagnostics, not in emitted symbol names. The 77-method Stage 6 inventory below records surviving callers and the exact unused/test-only queries without deleting them. The default suite passes 1301 tests (one ignored helper skipped), all 86 four-run R1 paths match, and workspace check, formatting and diff checks pass. Codegen has no warnings; the one checker-only warning is recorded for Stage 6. Logs: `/private/tmp/staple-5-10-step6-{default,ir}.log`. Step 7 is next.

### Step 7 — complete

The main plan now reports general current progress; the breakdown and substage plans separate historical migration results from the single-emitter commands and retired APIs. Current notes link to the 36-entry R2 verdicts and 77-method Stage 6 handoff, preserve the D2/D5 differences, and leave Stage 5.11 pending. The default suite passes 1301 tests (one ignored helper skipped), all 86 four-run R1 paths match, and formatting, plan-link and diff checks pass. Logs: `/private/tmp/staple-5-10-step7-{default,ir}.log`. Step 8 is next.

### Step 2 — test verdicts (R2)

The audit covers the original 31 entries (including the shared compiler assertion helper) and five additional tests found in source: **36 entries**. Verdicts are recorded before deletions. `convert (Step 3)` means the retained corpus harness is renamed and converted together with its shared definitions in Step 3.

| Entry | Verdict | Reason / surviving evidence |
| --- | --- | --- |
| `graph_validation::coroutine_and_reactive_thunks_bind_demand_driven` | keep | Already lowering-only; retains reachable-thunk and bound-site assertions. |
| `graph_validation::legacy_emissions_are_represented_in_the_graph` | delete | Legacy queue coverage only; `validate_specializations`, binding validators and `assert_catalog_census` enforce closed, materialized instances and catalog emission. |
| `graph_validation::stage_4_4_cleanup_matches_legacy_emission` | delete | Fixed-point re-expansion checks glue/finalizer plans and scanner sites; `check_owned_bindings` checks bound types and uniqueness; the scanner fixture asserts registration/storage/order, and `drop_order` pins scope-exit, move, replacement, temporary and nested registration behavior. |
| `graph_validation::stage_4_4_cleanup_matches_legacy_on_standard_library_values` | delete | Same validators; `drop_glue_shapes`, `buffers`, `finalizers`, coroutine and standard-library corpus programs retain strict emission and pinned behavior. |
| `graph_validation::stage_4_5_reactive_callback_environment_finalizer_is_planned` | convert | Retain the droppable callback capture's expanded, bound environment-finalizer assertion; drop the recorder check. |
| `graph_validation::stage_4_5_coroutine_body_ownership_matches_legacy` | convert | Retain empty scope ownership plus the frame binding's bound unwind glue; `coroutine_drop_order` independently pins cancellation order and the mirrored completion leak. |
| `graph_validation::stage_4_5_transition_matches_legacy_on_standard_library_values` | delete | `check_stage_4_5` re-expands frame/runner plans and validates creations; cleanup validators retain environment glue. Coroutine/reactive corpus fixtures pin execution. |
| `graph_validation::stage_4_6_extern_adapters_match_legacy_emission` | convert | Keep declaration arity, callable-use coverage, unused-extern exclusion and per-site dedup; these direct fixture assertions remain useful beyond the re-expansion validator. |
| `graph_validation::stage_4_6_unused_subsystems_record_no_requirement` | keep | Already lowering-only; keeps absence/presence assertions. |
| `graph_validation::stage_4_6_runtime_requirements_cover_legacy_surfaces` | convert | Inspect actual lowered LLVM symbol uses outside runtime internals/main, and compare program and catalog-owner requirements. |
| `graph_validation::stage_4_6_owner_requirements_match_each_emitted_function` | convert | Preserve independent LLVM-use checks for predeclared state cells in normal and loop bodies. |
| `graph_validation::formatting_sites_bind_their_helpers_and_callees` | keep | Already lowering-only; keeps constructor/write/finish/interpolation binding assertions. |
| `graph_validation::legacy_specialization_requires_the_concrete_callable_type` | delete | Tests the retired legacy matcher; concrete instance signatures and environments are checked by materialization/binding validators and catalog census. |
| `graph_validation::stage_5_3_catalog_instance_declarations_keep_legacy_llvm_types` | delete | `assert_catalog_census` checks every defined catalog entry's type/linkage and uniqueness, independently of legacy. |
| `graph_validation::stage_5_3_declaration_census_matches_legacy` | delete | Same catalog census; its legacy alias explanations are retired, while D5 distinct-artifact assertions survive. |
| `graph_validation::stage_5_3_main_matches_legacy_instruction_for_instruction` | delete | R1 preserves the whole module including main throughout cutover; corpus output and entry/root-region tests retain behavior checks afterward. |
| `graph_validation::stage_5_1_planned_names_match_legacy_declared_names` | convert | Check D2 directly: declared names when unreserved/available, ordinal fallback for reserved/duplicate/generic names. |
| `differential::stage_5_8_coroutine_creation_and_pair_bodies_match_legacy` | delete | Body comparison only; coroutine-plan validators, catalog census and coroutine corpus fixtures survive. |
| `differential::stage_5_8_awaits_and_block_on_match_legacy` | delete | Body comparison and suspension coverage; plan validation plus `coroutine_nested_child_awaits` and existing coroutine examples retain it. |
| `differential::stage_5_8_reactive_fixtures_match_legacy` | delete | Body comparison only; runner validators and resource/derived/capture corpus fixtures retain emission/behavior. |
| `differential::stage_5_8_schedulers_tasks_and_completions_match_legacy` | delete | Body comparison only; coroutine/game-loop examples and cancellation/wait/token corpus fixtures retain behavior. |
| `differential::stage_5_3_differential_harness_reports_and_matches_bodies` | convert (Step 3) | Retain strict emission, catalog census, structural-kind/body coverage, focus definitions and D5 distinct artifacts. |
| `differential::catalog_census_rejects_corrupted_emissions` | keep | Already lowered-only; retains injected extra/missing definitions and wrong-signature failures. |
| `lowered_emitter::selector_preserves_legacy_and_emits_reactive_body` | convert | Check default empty harness and reactive runtime call without comparing the selector. |
| `compile::stage_5_3_cli_differential_harness_compares_emitters` | convert (Step 3) | Retain strict compilation, object/link/run, pinned stdout, trap and exit-status assertions under the lowered default. |
| `layout::layout_context_agrees_with_checker_predicates` | convert | Hold the checker input locally and lower it; never recover it through the output bridge. |
| `compiler::assert_artifact_definition` | keep | Always assert planned catalog definitions; remove legacy spelling argument/arm from all callers. |
| `compiler::a_block_tail_coroutine_is_returned_instead_of_destroyed` | keep | Keep planned cleanup-pointer construction and absence of discard cleanup. |
| `compiler::adapts_non_variadic_externs_used_as_function_values` | keep | Always assert a defined planned adapter plus its native call. |
| `compiler::type_checks_generic_aliases_and_functions` | keep | Keep both planned identity specializations. |
| `modules::monomorphizes_imported_generic_functions_but_keeps_constructors_private` | keep | Keep planned specialization definitions and privacy checks. |
| `graph_validation::stage_4_5_coroutine_and_runner_transition_matches_legacy_emission` (additional) | delete | `check_stage_4_5` and fixed-point re-expansion cover fields, states, resource modes, capture finalizers and runner signatures. `coroutine_drop_order` pins unwind positions; D5 fixtures prove distinct pairs/runners. |
| `graph_validation::legacy_constructor_and_structural_bodies_match_artifact_plans` (additional) | delete | Fixed-point re-expansion and structural corruption tests validate adapter/structural decisions; corpus structural-kind/body coverage and pinned index/mutation/debug/iteration behavior survive. |
| `graph_validation::legacy_specializations_not_in_the_catalog_are_detected` (additional) | delete | Negative test of the retired legacy matcher; catalog/signature corruption tests and concrete graph validation survive. |
| `graph_validation::stage_4_7_census_accounts_for_every_emitted_function` (additional) | delete | Legacy-origin accounting is retired; the catalog census and all-artifact-families corpus preserve definition/type/linkage coverage. |
| `differential::normalization_compares_constants_by_content` (additional) | delete (Step 3) | Tests only the retired body-comparison constant normalization algorithm; no emitter behavior assertion is lost. |

**Drop/ownership audit:** the 4.4 comparison checks registration order and storage but cannot see emitted drop positions. `cleanup_scanner_records_sites_and_owned_bindings` directly checks value/cell registrations, nested/loop order and determinism; `drop_order` pins actual scope/early/propagation/break/continue/logical/match/move/replacement/call-temporary drops, including two loop locals. The 4.5 transition compares unwind drops as a set; `coroutine_drop_order` pins their emitted order. Its mirrored empty scope ownership assertion is retained as a converted test. Extern transition tests have no drop-position or ownership-registration assertions. No uncovered position is silently dropped.

## Starting Point (historical pre-cutover inventory)

The lowered emitter is complete and proven against legacy:

- the default, `lowered-emitter`, and `differential-shadow,lowered-emitter` gates each pass 1318 tests;
- the shadow run compared about 590 programs and 166k bodies, with only D5-explained differences;
- the 74-program corpus pins every runnable program's output;
- the legacy-free catalog census (`assert_catalog_census`) checks that every emitted function maps to exactly one catalog entry with the catalog's LLVM type and linkage.

What 5.10 removes:

| Area | Size and location |
| --- | --- |
| Legacy emitter | `codegen/mod.rs`, about 10950 lines. It holds about 280 `typed_module` reads and every symbol on the breakdown's `rg` list, and nearly nothing outside it does: one `TypedModule` doc mention each in `layout.rs`, `ir.rs`, `runtime.rs`, and `abi.rs`. |
| Legacy recorder | `codegen/legacy_recorder.rs` (278 lines), plus the `#[cfg(any(test, feature = "differential-shadow"))]` instrumentation inside `ModuleEmitter` |
| Differential comparison | `codegen/differential.rs` (2881 lines; the corpus definitions stay), and the legacy half of `lower/census.rs` (`census_mapping`, `assert_declaration_parity`, `LegacyCatalogEntry`, the legacy-origin coverage and D5 alias detection) |
| Selector and features | `Emitter`, `CodeGenerator::with_emitter`, the `lowered-emitter` feature (`staple-compiler` and `staple-cli`), `differential-shadow`, and the `cfg!(feature = "lowered-emitter")` branches (4 in `tests/compiler.rs`, 1 in `tests/modules.rs`, 1 in `codegen/mod.rs`) |
| Shared helpers only legacy calls | `add_finalizer_function`, `build_fn_type`, `coroutine_resource_bundle_type`, `variadic_argument_count_matches` (confirm with `rg` after the deletion) |
| `TypedModule` carried by lowering | `LoweredModule::typed` and the `typed: Box<TypedModule>` field (`lower.rs`; built with `Box::new(module.clone())`). The only remaining caller is `layout_context_agrees_with_checker_predicates`. |

Tests that touch legacy (31) and need a decision:

- **`lower/graph_validation.rs` (17):**
  - `coroutine_and_reactive_thunks_bind_demand_driven`
  - `legacy_emissions_are_represented_in_the_graph`
  - `stage_4_4_cleanup_matches_legacy_emission`
  - `stage_4_4_cleanup_matches_legacy_on_standard_library_values`
  - `stage_4_5_reactive_callback_environment_finalizer_is_planned`
  - `stage_4_5_coroutine_body_ownership_matches_legacy`
  - `stage_4_5_transition_matches_legacy_on_standard_library_values`
  - `stage_4_6_extern_adapters_match_legacy_emission`
  - `stage_4_6_unused_subsystems_record_no_requirement`
  - `stage_4_6_runtime_requirements_cover_legacy_surfaces`
  - `stage_4_6_owner_requirements_match_each_emitted_function`
  - `formatting_sites_bind_their_helpers_and_callees`
  - `legacy_specialization_requires_the_concrete_callable_type`
  - `stage_5_3_catalog_instance_declarations_keep_legacy_llvm_types`
  - `stage_5_3_declaration_census_matches_legacy`
  - `stage_5_3_main_matches_legacy_instruction_for_instruction`
  - `stage_5_1_planned_names_match_legacy_declared_names`
- **`codegen/differential.rs` (6):**
  - the four `stage_5_8_*_match_legacy` tests;
  - `stage_5_3_differential_harness_reports_and_matches_bodies`;
  - `catalog_census_rejects_corrupted_emissions`.
- **Elsewhere (8):**
  - `tests/lowered_emitter.rs::selector_preserves_legacy_and_emits_reactive_body`;
  - the CLI harness `stage_5_3_cli_differential_harness_compares_emitters`;
  - `codegen/layout.rs::layout_context_agrees_with_checker_predicates`;
  - the four `tests/compiler.rs` tests and one `tests/modules.rs` test with legacy-spelling `else` branches.

## Decisions Specific to 5.10

**R1: Parity across the cutover is lowered-IR identity.** Stage 5.10 must not change a single instruction of lowered output.

- Step 1 builds a reference binary from the pre-cutover tree with `--features lowered-emitter`.
- Every later step runs `scripts/compare-llvm-ir.py` against it, over the examples, the actual `game_loop` fixture, and every corpus program. It must report `same`.
- This replaces the body-for-body comparison with legacy during the stage, and needs no new snapshot machinery.
- After 5.10 there is no IR oracle, by design. Behavior is pinned by corpus stdout and the suite, structure by the validators and the catalog census.

**R2: Every legacy-touching test gets a recorded verdict.** Each of the 31 tests above is classified in this plan's Step 2 notes as one of:

- **keep:** it only needs a non-legacy fix, such as dropping a feature branch;
- **convert:** it asserts a plan-content or emission fact that no validator or census re-checks, so it is rewritten as a lowering-only or lowered-emission test;
- **delete:** its fact is already enforced elsewhere, which must be named, or it only compared against legacy.

No test is deleted without a verdict and its reason.

**R3: The corpus survives as a lowered-only suite.**

- `DifferentialExpectation` collapses to `MustRun` and `CompileOnly`. A former `LoweredOnly` entry becomes `MustRun` and keeps its D5 tag, so `assert_distinct_d5_artifacts` still proves the per-instance pairs and runners.
- The in-process harness keeps:
  - strict emission;
  - the catalog census;
  - the focus `emits` check: each listed template's instances must be defined, with planned names from the catalog;
  - the D5 distinct-artifact check.
- The CLI harness runs only the lowered emitter and asserts pinned stdout and `traps`.
- The names `differential` and `stage_5_3_*` no longer fit. Rename the module (for example `codegen/corpus.rs`) and the tests in the same step.

**R4: Delete in dependency order, compiling at every commit.** The order is: tests and harnesses that consume legacy, then the shadow feature and recorder, then the legacy census, then `ModuleEmitter` and the selector, then `LoweredModule::typed`. Each commit passes the gates in force at that point.

**R5: Behavior is untouched.** The mirrored defects (the D5 list and the 5.11 candidates) stay exactly as they are. Any behavior change found during 5.10 is a bug to fix by restoring the pre-cutover IR (R1), never a fix to keep.

## Steps

Each step ends with `cargo nextest run --workspace` (default features), the R1 comparison, formatting, and a commit. The shadow gate runs until Step 3 removes its feature; the `lowered-emitter` gate runs until Step 5 removes that feature.

### Step 1: Freeze the reference

- **Add a corpus dump.** It is a doc-hidden helper, plus an ignored test or tiny bin, that writes every corpus program to a directory as `name/main.sta` (file-based entries copy their file). R1 can then run over the corpus with the comparison script.
- **Build the reference binary** from the current tree with `--features staple-cli/lowered-emitter` into a scratch target directory outside `target/`, and record its commit.
- **Run the comparison of the reference against itself** with `--runs 4` over the examples, `game_loop`, and the dumped corpus. Every program must be a single variant, so later `same` results are meaningful. A program with more than one variant is a nondeterminism bug: fix it here, before anything is deleted.
- **Run the three 5.9 gates one last time** and record the shadow ledger totals as the final legacy evidence.

### Step 2: Make lowered the default and audit the tests

- **Flip the default.** `CodeGenerator::new` emits with the lowered emitter whatever the feature says. Legacy stays reachable only through `with_emitter(Emitter::Legacy)` and the shadow feature. R1 must be `same`, since the default binary is now the reference configuration.
- **Classify the 31 tests (R2).** Record a table with the verdict and its reason. The initial expectations below must be confirmed by reading each test:
  - **convert:**
    - `stage_5_1_planned_names_match_legacy_declared_names`: D2's rule becomes "a non-generic, unreserved instance's planned name is its declared `LoweredFunction::name`".
    - `stage_4_6_runtime_requirements_cover_legacy_surfaces` and `stage_4_6_owner_requirements_match_each_emitted_function`: every runtime symbol the lowered module references is covered by `LoweredRuntimeRequirements`, per program and per owner.
    - `stage_4_6_unused_subsystems_record_no_requirement`.
    - `stage_4_5_reactive_callback_environment_finalizer_is_planned`, `coroutine_and_reactive_thunks_bind_demand_driven`, and `formatting_sites_bind_their_helpers_and_callees`: their plan assertions stay, and only the legacy cross-check goes.
    - `layout_context_agrees_with_checker_predicates`: it keeps the `TypedModule` it lowered from instead of calling `LoweredModule::typed`.
    - `catalog_census_rejects_corrupted_emissions`: build its emission with the lowered emitter only.
  - **delete:** the census, declaration, and main-instruction comparisons (`stage_5_3_*`), now enforced by the catalog census and pinned behavior; the Stage 3.5 queue comparisons (`legacy_emissions_are_represented_in_the_graph`, `legacy_specialization_requires_the_concrete_callable_type`); the 4.4–4.6 "matches legacy" transition comparisons, after confirming that their plan facts are re-checked by the re-expansion validators; and the four `stage_5_8_*_match_legacy` body comparisons.
  - **keep:** the `tests/compiler.rs`/`tests/modules.rs` tests, minus their legacy `else` branches.

  For each 4.4–4.6 transition comparison, check specifically whether it asserts **drop positions or ownership registrations**. The Stage 5.5 review found the 4.4 comparison could not see drop positions, so the per-site body comparison was the only check. If a fact is unprotected, add a corpus program that pins it through output, as `drop_order` does.
- **Apply the verdicts**, convert first and then delete. The gates must stay green.

### Step 3: Retire the comparison harnesses (R3)

- **Collapse the corpus expectations.** Move the corpus and its in-process and CLI harnesses to lowered-only checks, then rename them.
- **Delete the body comparison:** `compare_fully_emitted_bodies`, `normalize_function`, `module_functions`/`module_constants` (unless the focus check still needs them), and the alias normalization.
- **Delete the `differential-shadow` feature,** `compile_shadow`, the process counter, and the ledger.

### Step 4: Retire the legacy census and recorder

Delete the legacy half of `lower/census.rs`: `census_mapping`, `assert_declaration_parity`, `LegacyCatalogEntry`, the legacy-origin coverage, the effect-omitting pair fallback, and the D5 alias detection. `assert_catalog_census` and its helpers stay. Delete `codegen/legacy_recorder.rs` and every `cfg(any(test, feature = "differential-shadow"))` block in `ModuleEmitter`.

### Step 5: Delete the legacy emitter and the selector

- Delete `ModuleEmitter` and everything in `codegen/mod.rs` that only legacy uses. `codegen/mod.rs` keeps `CodeGenerator`, the public result types, target-machine creation, and the module declarations.
- Delete `Emitter`, `with_emitter`, and the `lowered-emitter` features in both crates. Delete the `cfg!(feature = "lowered-emitter")` branches, keeping the lowered arm.
- Delete the shared helpers that became dead. Let the compiler's dead-code warnings confirm the list, and treat every warning as a deletion candidate, never an `#[allow]`.
- Consider moving `lowered/` up a level, since it is now the only emitter. A move is a pure rename: R1 `same`, and its own commit.

### Step 6: Remove `TypedModule` from lowering's output

- Delete the `typed` field and `LoweredModule::typed`. `Lowerer::lower` still borrows `&TypedModule` but no longer clones it into the output.
- **Mechanical check.** In `staple-compiler/src/codegen*`, `rg` finds none of: `TypedModule`, `typed_module`, `ResolvedModule`, `resolved()`, the `staple_syntax::{Expression, Item, Pattern, CallExpression, ProductExpression, RepeatedProductExpression}` imports, `SyntaxId`, `DefaultHasher`, `active_type_substitutions`, `expression_type_overrides`, `specialization_queue`, `infer_type_parameters`, `substitute_type`, `contains_type_parameter`, `standard_function_name_matches`, or `{:?}` inside a symbol name. The doc comments that mention `TypedModule` historically are reworded so the check is literal.
- **Stage 6 handoff list.** For every public or `pub(crate)` `TypedModule` method, record whether anything still calls it (lowering, diagnostics, the LSP, tooling). A method that only legacy codegen called is now unused. List them for Stage 6, which owns removing them; 5.10 does not.

### Step 7: Documentation

- Update the breakdown (5.10 section and status line) and [TYPED_LOWERING_PLAN.md](TYPED_LOWERING_PLAN.md) with:
  - what passed;
  - the R1 result;
  - the R2 verdict table, summarized with a link to it;
  - the observed differences: none beyond D2 names and the D5 list;
  - the Stage 6 handoff.
- Remove the now-false references to the selector, the shadow feature, and the legacy gates from the 5.x plans' "how to run" notes. Leave their history intact.

### Step 8: Gate

- `cargo nextest run --workspace` passes with no features. No other emitter or feature combination exists.
- **R1 is `same`** against the Step 1 reference over the examples, `game_loop`, and the full corpus. This is the breakdown's "no function-type changes and no duplicate instances": identical IR implies both, and the catalog census independently asserts unique definitions with catalog types.
- The corpus harnesses pass lowered-only. Every runnable entry's pinned output holds, and every D5 entry still proves distinct artifacts.
- The Step 6 mechanical `rg` check is clean, and `cargo build` reports no dead-code warnings in `codegen/`.
- The CLI `--emit llvm`, `--emit object`, and `run` paths pass with the worktree standard library on the examples.
- Formatting and `git diff --check` pass.
- **The migration is complete. Stage 5.11 follows.**

## Ordering

```text
Step 1 → Step 2 → Step 3 → Step 4 → Step 5 → Step 6 → Step 7 → Step 8
```

- **Strictly sequential.** Each deletion step removes something the next one would otherwise still have to compile around.
- **Step 2 is the only step needing judgment** (the R2 verdicts). Every later step is mechanical under R1.
- **Highest risk:** a transition test whose fact nothing else checks. That is why Step 2 inspects drop and ownership positions explicitly.
- **Disk.** Steps 1–2 build up to three feature trees, but after Step 5 a single configuration remains. Delete the scratch reference build at the end of Step 8.

## Stage 6 handoff: checked-program method inventory

All **77** public or crate-visible methods declared in `impl TypedModule` are listed below. Each live row gives a representative surviving caller (line numbers at Step 6); overloaded spellings such as `syntax`, `symbol_for`, and `function_by_id` were checked against the actual receiver. No production emission reads these queries; the layout agreement test keeps its own checker input. Methods remain in place for Stage 6. `drop_method_for` lost its sole production caller with legacy codegen and retains five lowering-test calls; Stage 6 can move it behind `cfg(test)` or replace those tests before removing it. `state_accesses_of_expression` has no callers and was already unused before cutover. `syntax`, `state_accesses_of_function`, `is_io_type`, and `is_task_type` retain only test uses (`is_io_type`/`is_task_type` already have `cfg(test)`). Other methods retain production use, including internal checker queries. The existing broad lowering-schema dead-code allowance is outside this cutover and remains a Stage 6 cleanup candidate; no allowance is added here.

| Method | Surviving use | Representative evidence |
| --- | --- | --- |
| `coroutine_plan` | lowering | `staple-compiler/src/lower.rs:3298` |
| `resolved` | lowering | `staple-compiler/src/lower.rs:2886` |
| `has_mutable_storage` | lowering | `staple-compiler/src/lower.rs:3179` |
| `is_mutated_parameter` | lowering | `staple-compiler/src/lower.rs:3213` |
| `is_move_parameter` | lowering | `staple-compiler/src/lower.rs:3214` |
| `syntax` | tests only | `staple-compiler/tests/compiler.rs:250` |
| `functions` | lowering | `staple-compiler/src/lower.rs:3097` |
| `implicit_thunks` | lowering | `staple-compiler/src/lower.rs:13563` |
| `implicit_thunks_in_id_order` | lowering | `staple-compiler/src/lower.rs:3099` |
| `derived_evaluators_in_symbol_order` | lowering | `staple-compiler/src/lower.rs:3236` |
| `trait_method_types_in_id_order` | lowering | `staple-compiler/src/lower.rs:2939` |
| `trait_parameter_arguments_in_id_order` | lowering | `staple-compiler/src/lower.rs:2934` |
| `trait_functional_dependencies` | lowering | `staple-compiler/src/lower.rs:2994` |
| `checked_trait_implementations` | lowering | `staple-compiler/src/lower.rs:3036` |
| `type_representation` | lowering | `staple-compiler/src/lower.rs:2918` |
| `type_parameter_templates` | lowering | `staple-compiler/src/lower.rs:2917` |
| `trait_prerequisites` | lowering | `staple-compiler/src/lower.rs:2993` |
| `semantic_ids` | lowering | `staple-compiler/src/lower.rs:2843` |
| `implicit_thunk_for` | lowering | `staple-compiler/src/lower.rs:4259` |
| `is_derived_symbol` | lowering | `staple-compiler/src/lower.rs:3177` |
| `derived_evaluator` | lowering | `staple-compiler/src/lower.rs:4145` |
| `function_by_id` | frontend analysis/diagnostics | `staple-compiler/src/ownership.rs:649` |
| `symbol_for` | lowering | `staple-compiler/src/lower.rs:3733` |
| `function_for` | lowering | `staple-compiler/src/lower.rs:6540` |
| `function_for_symbol` | lowering | `staple-compiler/src/lower.rs:3172` |
| `type_of_expression` | lowering | `staple-compiler/src/lower.rs:4271` |
| `product_default_plan` | lowering | `staple-compiler/src/lower.rs:5475` |
| `curried_default_plan` | lowering | `staple-compiler/src/lower.rs:6309` |
| `juxtaposed_call_plan` | lowering | `staple-compiler/src/lower.rs:6287` |
| `companion_type_of_expression` | LSP/tooling | `staple-cli/src/lsp/completion.rs:680` |
| `effects_of_expression` | lowering | `staple-compiler/src/lower.rs:4908` |
| `state_accesses_of_expression` | unused | No call sites; already unused before cutover (legacy had no call). |
| `state_accesses_of_function` | tests only | `staple-compiler/tests/compiler.rs:1011` |
| `resource_for_expression` | lowering | `staple-compiler/src/lower.rs:4039` |
| `coercion_for` | lowering | `staple-compiler/src/lower.rs:4911` |
| `propagation_for` | lowering | `staple-compiler/src/lower.rs:3785` |
| `match_for` | lowering | `staple-compiler/src/lower.rs:6151` |
| `string_formatting` | lowering | `staple-compiler/src/lower.rs:2873` |
| `logical_for` | lowering | `staple-compiler/src/lower.rs:5860` |
| `access_for` | lowering | `staple-compiler/src/lower.rs:4476` |
| `type_of_pattern` | lowering | `staple-compiler/src/lower.rs:4708` |
| `string_representation` | lowering | `staple-compiler/src/lower.rs:2870` |
| `is_copy_type` | checker query internals | `staple-compiler/src/typecheck.rs:1687` |
| `is_copy_in_function` | lowering | `staple-compiler/src/lower.rs:3981` |
| `is_io_type` | tests only | `staple-compiler/src/codegen/layout.rs:596` |
| `is_reactive_type` | lowering | `staple-compiler/src/lower.rs:3918` |
| `is_coroutine_type` | checker query internals | `staple-compiler/src/typecheck.rs:1651` |
| `is_task_type` | tests only | `staple-compiler/src/codegen/layout.rs:605` |
| `is_scheduler_type` | checker query internals | `staple-compiler/src/typecheck.rs:1652` |
| `is_tasks_type` | lowering | `staple-compiler/src/lower.rs:3920` |
| `is_wait_type` | frontend analysis/diagnostics | `staple-compiler/src/coroutine_lower.rs:123` |
| `is_resolver_type` | checker query internals | `staple-compiler/src/typecheck.rs:1654` |
| `is_completion_token_type` | checker query internals | `staple-compiler/src/typecheck.rs:1655` |
| `wait_result` | lowering | `staple-compiler/src/lower.rs:5148` |
| `coroutine_parts` | lowering | `staple-compiler/src/lower.rs:3317` |
| `task_result` | lowering | `staple-compiler/src/lower.rs:5140` |
| `io_resource` | lowering | `staple-compiler/src/lower.rs:2868` |
| `reactive_resource` | lowering | `staple-compiler/src/lower.rs:2869` |
| `entry_reactive_required` | lowering | `staple-compiler/src/lower.rs:2871` |
| `is_drop_method` | frontend analysis/diagnostics | `staple-compiler/src/ownership.rs:192` |
| `type_needs_drop` | lowering | `staple-compiler/src/lower.rs:3836` |
| `drop_method_for` | tests only; production now unused | `staple-compiler/src/lower/cleanup_artifacts.rs:2192` |
| `structural_trait_method` | lowering | `staple-compiler/src/lower.rs:6364` |
| `resolve_trait_obligation` | checker query internals | `staple-compiler/src/typecheck.rs:1689` |
| `moved_symbols` | lowering | `staple-compiler/src/lower.rs:4927` |
| `is_non_owning_symbol` | lowering | `staple-compiler/src/lower.rs:3210` |
| `is_borrowed_capture` | lowering | `staple-compiler/src/lower.rs:3413` |
| `type_of_symbol` | lowering | `staple-compiler/src/lower.rs:3194` |
| `declared_type_of_symbol` | lowering | `staple-compiler/src/lower.rs:3166` |
| `companion_type_of_symbol` | LSP/tooling | `staple-cli/src/lsp/completion.rs:422` |
| `is_companion_method` | LSP/tooling | `staple-cli/src/lsp/completion.rs:426` |
| `type_of_function` | lowering | `staple-compiler/src/lower.rs:3362` |
| `bounds_of_function` | lowering | `staple-compiler/src/lower.rs:3425` |
| `trait_dispatch_for` | lowering | `staple-compiler/src/lower.rs:3822` |
| `trait_impl_method` | lowering | `staple-compiler/src/lower.rs:6358` |
| `complete_trait_arguments` | lowering | `staple-compiler/src/lower.rs:6027` |
| `instantiated_trait_method_type` | lowering | `staple-compiler/src/lower.rs:6032` |
