# Stage 5.10 Plan: Cutover and Removal

This is the plan for the Stage 5.10 section of [STAGE_5_LLVM_MIGRATION_BREAKDOWN.md](STAGE_5_LLVM_MIGRATION_BREAKDOWN.md). First read:

- the breakdown's Migration Contract and Decisions D1–D6;
- the Stage 5.9 plan's handoff ([STAGE_5_9_FULL_SUITE_PARITY_PLAN.md](STAGE_5_9_FULL_SUITE_PARITY_PLAN.md), "Handoff to Stage 5.10" and "Post-gate review fixes").

This plan's gate (Step 8) supersedes the **Gate** paragraph in the breakdown's 5.10 section.

Line references are against `1501270` and will drift; re-locate code by name. Build with `CARGO_INCREMENTAL=0`: each feature combination gets its own build tree, and Stage 5.9 filled the disk.

## Execution Notes

### Step 1 — complete

- Pre-cutover production reference: `ee50922`, built with `CARGO_INCREMENTAL=0 cargo build --features staple/lowered-emitter --target-dir /private/tmp/staple-stage-5-10-reference`. Binary: `/private/tmp/staple-stage-5-10-reference/debug/staple`. The only source addition during this build is the test-only dump below.
- Added ignored `codegen::differential::tests::dump_corpus_sources`; invoke with `STAPLE_CORPUS_DUMP=/private/tmp/staple-stage-5-10-corpus cargo test -p staple-compiler dump_corpus_sources -- --ignored --nocapture`. It dumped all 74 entries and copies `.sta` companion modules for file entries.
- The final default, lowered, and shadow gates each pass all 1318 tests (one ignored dump helper skipped). Logs: `/private/tmp/staple-5-10-{default,lowered,shadow}.log`. Final legacy ledger: `/private/tmp/staple-5-10-shadow.ledger`, **589 programs, 166552 bodies, 8 D5-explained rejections**.
- Frozen reference for all later R1 gates: `/private/tmp/staple-stage-5-10-reference/release/staple`, built with `CARGO_INCREMENTAL=0 cargo build --release --features staple/lowered-emitter --target-dir /private/tmp/staple-stage-5-10-reference` from the same `ee50922` production source. The supplemental debug sweep also finished with 85 single-variant successes and the original macro-example rejection; the optimized reference makes repeated full-scope gates practical.
- Optimized reference self-comparison passes for **all 86 paths**, four runs per side, one variant each. The initial log `/private/tmp/staple-5-10-reference-release-self.log` contains 85 successes plus the pre-existing rejection of `examples/macros.sta`. That example used parenthesized effectful macro arguments which lowering rejected as positional products. Replacing the two invocations with block arguments makes the unchanged reference compile and run the complete intended tour. `/private/tmp/staple-5-10-reference-macros-self.log` proves its single stable variant; no path remains unverified. Compiler behavior is untouched. The underlying parenthesized-macro lowering rejection remains for a later front-end fix.
- The final test-only split into doc-hidden `dump_corpus` plus ignored `dump_corpus_sources` passes its focused ignored test; the 225 dumped `.sta` files are byte-identical to the original export. The macro example prints all eight intended lines, including `generated values are correct`. Its dependent LSP hover check passes; formatting and diff checks pass. Step 2 is next.

### Step 2 — preliminary audit findings (implementation not started)

The source inventory is broader than the initial 31-test list. Also audit `stage_4_5_coroutine_and_runner_transition_matches_legacy_emission`, `legacy_constructor_and_structural_bodies_match_artifact_plans`, `legacy_specializations_not_in_the_catalog_are_detected`, and `stage_4_7_census_accounts_for_every_emitted_function`. Their transition helpers and `instance_for_legacy_specialization` must be retired once their verdicts are recorded. `coroutine_and_reactive_thunks_bind_demand_driven` and `stage_4_6_unused_subsystems_record_no_requirement` already assert lowering facts without calling legacy; preserve their assertions.

## Starting Point

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

Each step ends with `cargo nextest run --workspace` (default features), the R1 comparison, formatting, and a commit. Until Step 5 deletes them, the `lowered-emitter` and `differential-shadow,lowered-emitter` gates also run.

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
