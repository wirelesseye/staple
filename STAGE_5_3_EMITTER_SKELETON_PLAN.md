# Stage 5.3 Plan: Finish the Lowered Emitter Skeleton and the Differential Harness

> **Current execution after Stage 5.10:** use `CARGO_INCREMENTAL=0 cargo nextest run --workspace` and `cargo check --workspace`, without emitter features. The permanent harnesses are `codegen::corpus::tests::corpus_emits_catalog_definitions` and `compile::tests::codegen_corpus_compiles_links_and_runs`. Selector, shadow and body-comparison descriptions below are implementation history; those APIs and gates have been retired. See the [cutover results](STAGE_5_10_CUTOVER_AND_REMOVAL_PLAN.md).

This plan covers what remains of Stage 5.3 as of `2b20b12`. It supersedes the Stage 5.3 **Gate** paragraph in [STAGE_5_LLVM_MIGRATION_BREAKDOWN.md](STAGE_5_LLVM_MIGRATION_BREAKDOWN.md); everything else in that section still applies. Read the breakdown's Migration Contract and Decisions D1–D6 first.

Line references are against `2b20b12` and will drift; re-locate code by function name.

**Status:** Complete. Step 1 landed the review fixes (F2 linkage, F3 coroutine pair names, F5 argument validation, F9 progress notes); Step 2 landed F8 (every body is attempted and every diagnostic reported) and partial emission (`compile_lowered_partial`, `LoweredEmissionReport`, stub bodies, and the family histogram; the empty program reports 157 stubs across 17 families); Step 3 landed the full declaration census over the Stage 4.7 corpus plus an every-artifact-family fixture (defined set, LLVM types, and linkage for 1492 mapped functions, with the syntax-aliased coroutine entries as the only defined-set delta); Step 4 landed F7 (effect-row resource parameters and mutable-parameter pointers), the explicit capture kind/layout helpers, the initializer storage sequence, and the instruction-for-instruction `main` comparison; Step 5 landed F6 (the C-string, String, and integer builders are shared `Backend` operations, and the sharing rule is Migration Contract item 7) with the pre-step IR comparison unchanged over the examples and probes; Step 6 landed the differential harness (shared corpus in `codegen/differential.rs`, 2278 fully emitted bodies compared in-process (after the review added `game_loop`; constants compare by content), and the CLI blocked/identical report) and fixed the two emitter gaps it surfaced (`Stored` closure loads and name-read order); Step 7 recorded the revised gate and the 5.4–5.6 handoff in the breakdown. The revised gate is met: strict emission reports every unsupported site, every corpus program and the standard library emit a verified partial-mode module, the declaration census holds, every fully emitted function's normalized body matches legacy, the CLI harness reports every corpus program as blocked or identical with none different, `rg` finds no lowered-module-contract violation in `codegen/lowered/`, and all Contract 6 gates pass. The runnable-corpus requirement moves to 5.6. A post-gate review fixed six gaps: constants now compare by content in body normalization, the lowered function environment keys resources by provider, corpus entries carry a `CompileOnly`/`MayBeBlocked`/`MustRun` expectation that the CLI harness enforces, the corpus includes `game_loop`, stub bodies detach uses before deletion, and the IR comparison script no longer reports sampling gaps as `DIFF`. See **Post-gate review fixes** in the breakdown's Stage 5.3 section.

## Where Stage 5.3 Stands

Landed in `e29770a`, `9f8b989`, `535291e`, `2b20b12`, and verified in review:

- The D1 `Emitter` selector (`CodeGenerator::with_emitter`) and the `lowered-emitter` feature on both crates. `cargo check --workspace --features staple-compiler/lowered-emitter` is clean.
- `codegen/lowered/mod.rs` with `LoweredEmitter`, holding only an `EmissionView` and the shared `Backend`. It contains no `TypedModule`, `resolved()`, `SyntaxId`, or AST types.
- Runtime installation gated by `LoweredRuntimeRequirements`, with the garbage collector always installed (D6).
- Declarations for instances (planned names, closure ABI), artifacts (the pair as two functions, drop glue as none), externs (with arity suffixes), module storage, initialization-state and signal/derived metadata globals, and initializers.
- Initializer emission with entry IO/reactive resources and reactive-scope disposal, and a `main` harness built from `global_root` and initialization order.
- An owner dispatcher over instance and initializer arenas with exhaustive item and expression matches.
- `stage_5_3_catalog_instance_declarations_keep_legacy_llvm_types`, which compares declaration types for eager and specialized instances, initializers, constructor adapters, and structural methods, and checks the declaration count against the catalog.

Also landed, but this is Stage 5.4/5.5 scope (see finding F1): direct, indirect, and extern calls, the integer-arithmetic and C-string intrinsics, simple callable values, C-string literals, simple bindings and patterns, and a never-returning loop.

An empty program on the new emitter stops at `std/fmt.sta:110` ("coercion or move").

## Review Findings

**F1 (plan defect, blocking): the 5.3 gate cannot pass in 5.3.** Every program loads the standard library, and there is no mode without it. The catalog makes every non-generic standard-library function an eager root, exactly as legacy emits it. An empty program therefore defines about 280 non-runtime functions: legacy defines 348 in total, most of them standard-library bodies. Emitting them needs patterns, matches, products, strings, formatting, cleanup, and more, which is most of 5.4–5.6. The original gate ("an empty `main` … passes the differential harness", meaning it runs) is therefore unreachable until those substages land. The implementation has been adding 5.4/5.5 families one at a time to chase it. **Resolution:** Step 2 (partial emission for the harness only) and the revised gate below. The call, intrinsic, and pattern code that has already landed stays, but no further construct-family work happens in 5.3. The 5.4/5.5 plans take ownership of that code.

**F2: linkage differs from legacy.** The declaration test compares LLVM function types only, not linkage.

| Entry | Legacy | Lowered |
| --- | --- | --- |
| Generic instance (ordinal name) | `Internal` (`ensure_function_specialization`) | default (external) |
| Eager non-generic instance | default | default |
| Structural method | default (`structural_trait_method_code` passes `None`) | `Internal` |
| Constructor adapter, extern adapter, runner, coroutine pair | `Internal` | `Internal` |
| GC finalizer | default | default |

**F3: the backend builds some catalog function names.** Coroutine pairs are declared as `{planned}_resume`/`{planned}_cleanup`, a name the catalog never collision-checks. D2 says the backend reads names and never builds them. Module globals (`unique_global_name` adds `.global.{symbol}`) and initializer names are data or fixed-scheme names, which the Stage 4.1 negative matrix allows to stay backend-local. They do avoid planned names, which is correct, but the rule is not written down.

**F4: artifacts are declared `Internal` without a body.** Once instance bodies emit, any artifact that is still bodiless makes the module fail verification (an internal declaration needs a definition). This does not show today only because emission aborts earlier.

**F5: some call routes silently skip work (violates Contract 2).** In `emit_call`, `direct_value_route` (native extern calls, `StringFromCString`/`StringToCString`, and the `String` constructor) bypasses the checks on argument pass mode, `temporary`, `writeback`, and `drops_after_call`. A `drops_after_call` argument on those routes is silently not dropped, and a `writeback` is silently skipped. An unported fact must produce a diagnostic, never be skipped. Separately, native variadic extern calls do not apply legacy's variadic argument handling (`compile_arguments(.., is_var_arg)`).

**F6: legacy code was copied instead of shared.** The C-string literal (`build_owned_c_string`), `string_from_c_string`, `string_to_c_string`, and integer arithmetic are re-implementations of legacy code in `codegen/mod.rs`, with different SSA value names (`.arithmetic` instead of `.add`/`.subtract`/…). Copies drift, and differing names add noise to body-level IR comparison.

**F7: parameter binding is incomplete.** `bind_parameters` skips the effect-resource parameters (positions `1..=resource_count`) instead of binding them as the body's `function_providers`. Calls with resource bindings are rejected, so no wrong code results yet. Parameter destructuring is diagnosed. Mutable-parameter pointers (`bind_mutable_parameter_pointers`) are not ported.

**F8: emission aborts at the first unsupported site.** Only one diagnostic is reported per compile, so there is no measure of progress or remaining work.

**F9: the progress notes are duplicated.** The Stage 5.3 section of the breakdown contains three overlapping progress paragraphs, and the Stage 5 section of the main plan contains two. They should be merged into one status note.

**F10: the declaration comparison does not cover every family.** Finalizers, runners, coroutine pairs, and extern adapters are not compared. The review read the legacy declarations and found their types match, but nothing enforces that.

## Steps

Each step ends with the Contract 6 gates, where applicable, and a commit.

### Step 1: Fix the review findings in the existing code

- **F2:** give generic (ordinal-named) instances `Internal` linkage and structural methods default linkage, matching legacy. Record the linkage rule per family in a comment on `declare_artifacts`/`declare_instances`.
- **F3:** make the catalog name both pair functions. Add `LoweredProgram::planned_coroutine_pair_names(ordinal) -> (resume, cleanup)`, include both names in the collision-checked planned set, and read them in `declare_artifacts`. Record in the breakdown that module globals, initialization-state and metadata globals, and initializers use fixed backend naming and must avoid every planned name, which `unique_global_name` guarantees.
- **F5:** remove the `direct_value_route` bypass. For those routes, accept exactly what legacy does (the `CString` argument with `c_string_temporary`, and a moved `CString` in the conversions) and diagnose any other pass mode, `temporary`, `writeback`, or `drops_after_call`. Diagnose variadic native extern calls until 5.4 ports the variadic argument handling.
- **F9:** merge the progress paragraphs in both plan files into one status note each.

### Step 2: Report every unsupported site, and add partial emission for the harness

- **F8:** emit every body even after an earlier one fails, and collect all diagnostics. `compile` still returns `Err(all diagnostics)` when any body failed.
- **Partial mode.** Add `#[doc(hidden)] pub struct LoweredEmissionReport` and a `#[doc(hidden)]` entry point, `CodeGenerator::compile_lowered_partial`, used only by the differential harness and never by the CLI or the `lowered-emitter` default:
  - When a body fails, delete its basic blocks and emit a stub body: `llvm.trap` followed by `unreachable`. Record the function's planned name, its catalog entry (instance, artifact, or initializer), and the diagnostic.
  - Every artifact whose family has no body emitter yet also gets a stub (this resolves F4 for the harness).
  - `main` is never stubbed.
  - The report lists the stubbed functions and a histogram of diagnostics by construct family (the text in `lowered emitter: <family> is not implemented yet`). It is the progress measure for 5.4–5.8.
- Invariant: a partial-mode module passes LLVM verification. A test checks this for the empty program and every corpus program.
- Production emission never stubs. A test runs `CodeGenerator::with_emitter(.., Emitter::Lowered)` on a program with unsupported sites and requires `Err` with more than one diagnostic.

### Step 3: Declaration parity for every catalog entry

Extend `stage_5_3_catalog_instance_declarations_keep_legacy_llvm_types`, or add a sibling test, into a full census over a corpus: the Stage 4.7 census programs plus a fixture using every artifact family. It maps each legacy-defined function to its catalog entry through the Stage 4.7 census machinery (`LegacyFunctionOrigin` → instance or artifact → planned name, with pairs through the Step 1 pair names). Then it requires:

- the set of functions the lowered module *defines* in partial mode, whether as real bodies or stubs, equals the mapped legacy set plus `main` and the UTF-8 validator, minus the census's explained items (inlined drop glue, coroutine bodies inside `resume`, eager but unused extern adapters, and entries reachable only through syntax-aliased coroutines, which the lowered module additionally defines);
- identical LLVM function types for every mapped pair, including finalizers, runners, coroutine pairs, and extern adapters (F10);
- identical linkage for every mapped pair (F2).

Legacy's runtime-module functions are excluded on both sides.

### Step 4: Complete the parts of 5.3 that belong to the emitter itself

- **F7:** bind the resource parameters to the body's `function_providers` in effect-row order, keeping each one's indirect/borrowed fact, so that `LoweredResourceUse` reads and call `resource_bindings` can find them in 5.4. Port `bind_mutable_parameter_pointers` from the parameter records, and cover whole-parameter (`CheckedMutation::Whole`) and indirect parameters.
- Captures: cover every capture storage kind in `LoweredInstanceCapture` (value, cell, borrowed, mutable-parameter pointer, initialization-state, derived) with the same field layout as legacy `build_capture_environment`. Stage 4.4's `ClosureEnvironment` finalizer plan already fixes the order.
- Parameter destructuring stays a diagnostic here; it is pattern work, and moves to 5.5 (update the 5.5 section to say so).
- Initializers: every item kind whose semantics are purely about storage (the binding state sequence 1 → 2, generic bindings, module-global stores) matches legacy `compile_top_level_item`. Other item kinds stay diagnostics.
- `main`: confirm it matches legacy `compile_main_function` instruction for instruction, apart from the `HashMap`-ordered root registrations, which are now ordered by symbol.

### Step 5: Share legacy helpers instead of copying them

Set the rule for 5.3–5.8: **when the new emitter needs a legacy operation that does not read `TypedModule` or the AST, move the operation into the shared `Backend` layer (as Stage 5.2 did) and have both emitters call it. Never copy it.** This gives both emitters the same instructions and names, which makes body-level comparison (Step 6) feasible.

- Apply the rule to the F6 copies now: move `build_owned_c_string`, the core of `compile_string_from_c_string`/`compile_string_to_c_string`, and the integer binary/compare builders into `Backend`, with value names taken from legacy. Switch legacy to the shared versions.
- Prove legacy IR is unchanged with `scripts/compare-llvm-ir.py` against the pre-step binary, over the examples and the Stage 5.2 probe programs.

### Step 6: Differential harness

- **In-process harness** (`#[cfg(test)]` module `codegen/differential.rs`, so it can use the legacy recorder and the census mapping). For each corpus program:
  - emit with legacy and with lowered partial mode, and verify both modules;
  - run the Step 3 declaration census (defined set, types, linkage);
  - for every function the lowered emitter **fully** emitted (no stub), compare its normalized body with the mapped legacy function. Normalize by renaming symbols through the census map, renumbering SSA values and block labels in order of appearance, and sorting `__staple_gc_register_root` calls. A difference fails unless the function is on an explicit, commented list of explained differences. Stage 5.3 adds none.
  - print the partial-mode report, with the stub count and the histogram of families.
- **CLI harness** (in `staple-cli/src/compile.rs` tests, through `CodeGenerator::with_emitter`): compile, link, and run each corpus program with both emitters, then compare stdout and exit status. A program whose lowered compile fails in strict mode is reported as **blocked** (with the Step 2 family histogram) rather than failed. A program whose lowered compile succeeds must behave identically. After F1, no corpus program is expected to run in 5.3. The CLI harness starts gating as soon as the empty program compiles strictly, which is expected after 5.6, and from then on a program that once ran must never regress to blocked or different.
- **Corpus list:** one ordered list in the harness module, shared by both harnesses, with each entry tagged by the substage that added it. 5.3 adds the empty program, a non-generic integer-arithmetic program, a program with module globals and initialization state across two modules, the Stage 4.7 census programs, and `staple-compiler/examples/*.sta` (excluding `macros.sta`, which fails in lowering). Later substages only append.

### Step 7: Gate and handoff

**Revised gate:**

- Step 1's findings are fixed.
- Strict emission reports all unsupported sites.
- Every corpus program and the standard library emit a partial-mode module that passes LLVM verification.
- The declaration census holds (defined set, function types, linkage).
- Every fully emitted function's normalized body matches legacy.
- The CLI harness runs and reports each corpus program as blocked or identical, with none different.
- `rg` finds no `typed_module`, `TypedModule`, `resolved()`, `staple_syntax::Expression`, or `SyntaxId` in `codegen/lowered/`.
- The Contract 6 gates pass, including `cargo check --workspace --features staple-compiler/lowered-emitter`.

The runnable-corpus requirement of the original gate moves to the first substage after which the empty program compiles strictly (expected 5.6). Record that in the 5.6 gate.

**Handoff:**

- Record the partial-mode report for the empty program and the full corpus: stub count, and the family histogram with the top families. Those counts are the starting backlog for the 5.4, 5.5, and 5.6 plans.
- Update the breakdown: replace the 5.3 gate with the revised gate, add the Step 5 sharing rule to the Migration Contract, note in 5.4 that it takes over the call and intrinsic code already landed and the variadic extern argument handling, and note in 5.5 that it takes over parameter destructuring and the existing pattern and loop code.
- Update [TYPED_LOWERING_PLAN.md](TYPED_LOWERING_PLAN.md) with a single status note.

## Ordering

```text
Step 1 → Step 2 → Step 3 ─┐
          Step 4 ─────────┼→ Step 6 → Step 7
          Step 5 ─────────┘
```

Steps 3, 4, and 5 are independent once Step 2 lands. Step 5 only touches the shared layer and legacy call sites, so it can run in parallel with Step 4. Step 6 needs the partial mode (Step 2), the census (Step 3), and the shared helpers (Step 5), because body comparison fails on copied code.
