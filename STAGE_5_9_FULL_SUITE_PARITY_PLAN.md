# Stage 5.9 Plan: Full-Suite Parity and New-Emitter Census

This is the plan for the Stage 5.9 section of [STAGE_5_LLVM_MIGRATION_BREAKDOWN.md](STAGE_5_LLVM_MIGRATION_BREAKDOWN.md). First read:

- the breakdown's Migration Contract (items 1–7) and Decisions D1–D6, especially D2 (planned names) and D5 (mirrored defects);
- the post-gate review fixes in the 5.6–5.8 plans and sections.

This plan's gate (Step 9) supersedes the **Gate** paragraph in the breakdown's 5.9 section.

Line references are against `f19f4d9` and will drift; re-locate code by function name.

## Starting Point

Stages 5.3–5.8 closed against the differential corpus. It has 62 programs, compares 16350 fully emitted bodies with zero stubs, and its CLI harness has 54 identical, 3 compile-only, and 5 lowered-only programs. The corpus is not the whole suite, though. With the lowered emitter as the default,

```bash
cargo nextest run --workspace --no-fail-fast --features staple-compiler/lowered-emitter
```

1312 tests run and **30 fail**, all in `tests/compiler.rs` and `tests/modules.rs`. There are two kinds of failure.

**Real emission bugs: 14 tests, 8 causes.** The corpus never reached these. Each is a construct the lowered emitter mishandles.

| Cause | Tests | Symptom |
| --- | --- | --- |
| A. `SliceRef` coercion receives a non-pointer | `delegates_indexing_through_refs_to_the_payload`, `derives_trait_delegated_product_indexing`, `indexes_slices_through_the_standard_library_implementation`, `iterates_slices_through_the_standard_library_implementations`, `compares_slices_and_strings_through_the_standard_library_eq_implementations`, `supports_repeated_spread_and_slice_references` | "invalid fixed reference representation" from `emit_coercion`'s `SliceRef` arm |
| B. Shared-cell capture before its cell exists | `applies_initialization_state_to_recursive_local_generics`, `captures_potentially_unsafe_local_defs_by_binding_cell` | "closure capture storage is not available" (`capture_field_value`). Likely legacy's `predeclare_checked_bindings` cell predeclaration for local generics and `def`s is missing. |
| C. Repeated-product return | `natural_count_parameters_drive_repeated_product_values` | invalid module: `ret i32 %capture` in a function returning `<{ i32, i32, i32 }>` |
| D. Effect-polymorphic closure call | `specializes_generic_effect_parameters` | invalid module: wrong argument count in `%closure.call` (hidden resource arguments after effect substitution) |
| E. Curried multi-parameter trait call | `type_checks_product_and_curried_multi_parameter_traits` | `incomplete call`: argument slots unfilled |
| F. `return` from a nested expression block | `returns_from_nested_expression_blocks` | invalid module: `entry` in `answer` has no terminator |
| G. Planned initializer name collision | `sibling_blocks_may_reuse_the_same_submodule_name` | ``initializer name `__staple_init_mmain.foo` collides with a function``; D2's collision rule does not cover initializer names |
| H. Coroutine pair missing from a block-tail `coro` | `a_block_tail_coroutine_is_returned_instead_of_destroyed` | "the block tail should construct a coroutine frame". Confirm in Step 1 whether this is only the legacy `@__staple_coro_` name in the assertion (then it belongs to the next table) or missing construction. |

**IR-text assertions on legacy-only spellings: 16 tests.** These are expected under D2 and need no emitter fix (with H possibly joining them):

| Spelling | Tests |
| --- | --- |
| legacy artifact names: `__staple_structural_Debug`, `structural_IntoIterator`, `__staple_gc_finalize_{closure,cell,buffer}_`, `__staple_gc_finalize_`, `__staple_extern_puts` | `derives_debug_for_nominal_representations`, `provides_formatter_display_debug_and_structural_product_debug`, `provides_structural_debug_for_sum_types`, `derives_structural_iteration_for_products`, `dropping_an_unstarted_coroutine_emits_a_cleanup_call`, `moves_resources_into_managed_closures_and_borrows_ref_payloads`, `lowers_move_only_mutation_reinitialization_and_captured_cells`, `buffer_intrinsics_type_check_and_compile`, `lowers_custom_drop_and_gc_finalizer_glue`, `adapts_non_variadic_externs_used_as_function_values` |
| legacy generic-instance names (`identity__{hash}`; planned names are `__staple_instance_N`) | `type_checks_generic_aliases_and_functions`, `monomorphizes_imported_generic_functions_but_keeps_constructors_private` |
| legacy parameter value names (`%value`, `%a`, `%x`, `%args`); the lowered emitter leaves parameters unnamed | `treats_singleton_products_as_their_element`, `generates_a_named_function_after_predeclaring_it`, `destructures_nested_product_patterns`, `binds_whole_copy_values_while_destructuring_them` |

**Other inputs:**

- **Twelve "not implemented yet" sites** remain in `codegen/lowered/mod.rs`. Eleven are the per-family diagnostic closures (`unsupported`/`unimplemented`), and one is `call argument writeback` (lowering never records a writeback).
- **The partial-mode machinery** (`compile_lowered_partial`, the stub report, reached families, `FAMILY_OWNERS`, `COMPLETED_SUBSTAGES`) exists only to measure unported families, and none remain.
- **Declaration census.** The Stage 4.7/5.3 census (`assert_declaration_parity`) maps lowered definitions to *legacy* functions and compares LLVM types and linkage. Stage 5.10 deletes legacy, so 5.9 adds a census that needs only the catalog.

## Decisions Specific to 5.9

**P1: Fix every failure where its fact lives (Contract 1).** A real failure is fixed in the lowered emitter when the emitter mis-reads a record. It is fixed in lowering, with a validator and a corruption test, when a fact is missing. It is never fixed by consulting `TypedModule` or by changing the test's expectation. A real failure's test assertions stay as they are; only the spelling-only tests in the second table change.

**P2: Name parameters the way legacy does.** Legacy calls `value.set_name(&binding.name)` for each parameter bound by a plain binding pattern (`bind_pattern_value`, skipped when the pattern carries a type annotation); other parameters stay numbered.

- The lowered emitter mirrors this from the parameter pattern's recorded binding names.
- LLVM value names are not symbols, so D2 does not apply. The body comparison already normalizes value names, so nothing there changes.
- This keeps IR readable and removes the four parameter-name assertion failures without editing them.

**P3: Assertions name catalog-planned symbols through a helper, never a hard-coded index.**

- Generic-instance and artifact names come from the catalog (D2), and indices such as `__staple_instance_23` are unstable.
- Add a `#[doc(hidden)]` test helper on `LoweredModule` that returns the planned names of a template's instances, or of an artifact family (for example `planned_instance_names("identity")` and `planned_artifact_names(StructuralDebug)`). Tests assert against those.
- Where a test only needs "a structural `Debug` body exists", it asserts on the stable planned-name prefix (`__staple_structural_debug_`, `__staple_gc_finalizer_`, …) from D2, after checking the prefix against the catalog's naming code.

**P4: Every program the suite compiles is a differential program.** The 30 failures show that a hand-curated corpus misses constructs. While legacy still exists, add a shadow comparison:

- Behind a test-only cargo feature (`differential-shadow`), `CodeGenerator::compile_module` also emits the module with the other emitter.
- It then runs the declaration census and the normalized body comparison, and panics on any unexplained difference.
- Running the whole suite with the feature makes every `tests/compiler.rs`, `tests/modules.rs`, and CLI program a differential program. This is the breakdown's "full union corpus" in practice.
- It reuses the corpus harness's helpers, made reachable from integration tests through the `#[doc(hidden)]` `differential` module, not copied.

**P5: Partial mode and the family tables go away in 5.9, not 5.10.** Once no family stubs, `compile_lowered_partial` and its reports measure nothing.

- The harness switches to strict emission (`compile_module` with `Emitter::Lowered`) and keeps its body comparison.
- The per-family diagnostic closures become internal-invariant errors (or are deleted where the match is now exhaustive).
- `FAMILY_OWNERS`, `COMPLETED_SUBSTAGES`, the reached-family census, and the partial-mode tests are removed. Their guarantee becomes "strict emission succeeds for every program", which the suite already enforces.
- This keeps 5.10 a pure deletion of legacy.

**P6: Freeze the oracle before 5.10 needs it.** Every runnable corpus entry gets an `expected_stdout` captured from legacy (and an exit status where it traps), so the corpus still means something once legacy is gone. Entries whose output depends on GC timing pin only deterministic lines, or are marked compile-only with a comment.

## Steps

Each step ends with both suite gates and a commit:

```bash
cargo nextest run --workspace
cargo nextest run --workspace --features staple-compiler/lowered-emitter
```

The default gate must stay green throughout. The feature gate's failure count must only fall, and each step's notes record it.

### Step 1: Baseline and triage

- **Reproduce the 30 failures** and record each one's root cause, refining the cause table above. Confirm cause H, and split any cause that turns out to be two.
- **Reduce each real failure to a minimal program** and add it to the differential corpus as a `5.9` entry: `MayBeBlocked` until fixed, then `MustRun` with `expected_stdout` where it runs. The corpus then covers what the suite found.
- **Record the "not implemented yet" sites** and confirm that `writeback` is unreachable: lowering never sets `LoweredCallArgument::writeback`. If so, delete the field and its arm. If not, implement it under P1.

### Step 2: Parameter names (P2)

Mirror legacy's naming rule in the lowered emitter's parameter binding. **Gate:** the four parameter-name tests pass unchanged, and the corpus body comparison is unchanged.

### Step 3: Real failures A–H (P1)

Fix each cause in its own commit, smallest blast radius first: G (naming), F (control flow), then E, D, C, B, and A last (six tests; likely in coercion lowering or the `Ref`/slice place path). For each cause:

- record the root cause and where the fix landed (emitter or lowering);
- flip its Step 1 corpus entry to `MustRun`;
- show that the body comparison still holds.

A fix in lowering adds its validator check and a corruption test.

### Step 4: IR assertions (P3)

Add the planned-name test helpers and update the spelling-only tests. Where a test's legacy spelling encoded a legacy-only shape (D5), state the reason in the test, as the breakdown requires. **Gate:** with Steps 2–3, the feature gate has zero failures.

### Step 5: Suite-wide shadow comparison (P4)

- Add the `differential-shadow` feature and the `compile_module` hook. Run the whole suite with it.
- Fix every unexplained difference under P1 and add each reduced program to the corpus.
- Record the number of programs and bodies the shadow run compared.
- The D5 explanations (aliased pairs/runners, legacy rejections) come from the existing census logic. Any new explanation category must be named and justified in the step notes.

**Gate:** the suite passes with `--features staple-compiler/differential-shadow`.

### Step 6: Retire partial mode and the family tables (P5)

- Switch the harness to strict emission.
- Delete the partial-mode entry point, stub report, reached-family census, `FAMILY_OWNERS`, `COMPLETED_SUBSTAGES`, and their tests. Rewrite the `tests/lowered_emitter.rs` partial-mode tests as strict-emission tests, or delete them if they only exercised stubbing.
- Turn the remaining diagnostic closures into internal-invariant errors.

**Gate:** `rg "not implemented yet" staple-compiler/src/codegen/lowered` is empty.

### Step 7: The new-emitter census

Add a permanent census that needs no legacy. For every program in the corpus (and in the shadow run):

- **Every defined function maps to exactly one catalog entry.** Each function the lowered module defines outside the runtime modules is the planned name of exactly one catalog instance or artifact, or is `main` or the UTF-8 validator.
- **Every catalog entry is defined or explained.** The allowed explanations are: drop glue (inlined, D3), coroutine body-thunk instances (emitted inside their pair's `resume`), and instances that are declared but legitimately bodiless (state the rule).
- **The defined function's LLVM type equals the catalog signature's compiled type.** This preserves the type guarantee of the 4.7 census without legacy.

Run it alongside the legacy census until 5.10 deletes the latter.

### Step 8: Freeze the oracle (P6)

- Capture legacy's stdout and exit status for every runnable corpus entry that lacks `expected_stdout`, and pin them.
- Add an assertion that every `MustRun` entry has pinned output, so 5.10's harness can drop the legacy comparison and keep the behavior check.

### Step 9: Gate and handoff

**Gate:**

- Both suite gates pass: the default emitter and `--features staple-compiler/lowered-emitter`.
- The suite passes with `--features staple-compiler/differential-shadow`, with only D5-explained differences. The program and body counts are recorded.
- The new-emitter census passes over the corpus and the shadow run. The legacy declaration census still passes, including identical LLVM function types for every mapped function.
- `rg "not implemented yet" staple-compiler/src/codegen/lowered` is empty, partial mode and the family tables are gone, and every `MustRun` corpus entry pins its output.
- The CLI `--emit llvm`, `--emit object`, and `run` paths pass under both emitters with the worktree standard library.

**Handoff to 5.10:**

- The list of tests and helpers that depend on legacy, so 5.10 can audit them. It should be split into those the new census or pinned output already replaces and those asserting plan-content facts no validator re-checks.
- The final corpus and shadow-run numbers.
- Update [TYPED_LOWERING_PLAN.md](TYPED_LOWERING_PLAN.md) and the breakdown's status line.

## Ordering

```text
Step 1 → Step 2 → Step 3 → Step 4 → Step 5 → Step 6 → Step 7 → Step 8 → Step 9
```

- Steps 2–4 are independent of each other once Step 1 has triaged, but finishing them first makes Step 5's shadow run start from a green feature gate.
- Step 5 comes before Step 6 because the shadow run may expose new families of failure. Strict emission reports those as ordinary errors, so the family tables are not needed to find them, but keeping them until Step 6 avoids churn.
- Step 7 can be written alongside Step 5; its gate runs after Step 6.
- **Highest-risk item:** cause A (six tests in the slice/`Ref` coercion path, possibly a missing lowering fact) and whatever Step 5 finds. Keep each fix in its own commit with its corpus entry.

## Implementation progress

### Step 1 — baseline and triage (complete)

Nine reduced `5.9` entries extend the differential corpus from 62 to 71 programs. They cover A–H, with separate recursive-local-def and recursive-local-generic fixtures for B. Only these new triage entries may be `MayBeBlocked`; existing entries keep the Stage 5.8 ratchet. The harness checks legacy compilation before accepting a blocked triage fixture.

Root-cause audit:

- A: lowered name loads use the expression's coerced target type to load storage; `SliceRef` then receives a slice struct instead of the stored `Ref` pointer. Fix belongs in emission.
- B: `emit_block` lacks legacy's predeclaration of initialization-checked local def cells, so a recursive closure captures a cell before it is allocated. Fix belongs in emission.
- C: instance cloning substitutes the symbolic repeated-product count but retains `collapsed`; emission consequently returns one element. Concrete count/collapse recomputation and validation belong in lowering.
- D: `rebind_hidden_resources` rebuilds resource steps only for intrinsic calls. Effect substitution can add resources to an indirect closure call without adding its ABI argument steps. Fix and validation belong in lowering.
- E: whole-product call arguments can occupy slot zero while the concrete parameter flattens to multiple slots; the emitter's unpack fallback only accepts an unplaced argument. Verify the concrete slot record and correct the owning lowering/emission rule.
- F: the call argument path continues after evaluating a nested `return`; subsequent instructions can follow the terminator. Fix belongs in emission.
- G: initializer declaration rejects repeated module prefixes instead of assigning a collision-free recorded name. Record and validate deterministic initializer names in lowering.
- H: construction works (the lowered CLI block-tail-coroutine execution test passes); the compiler assertion hard-codes legacy's `store ptr @__staple_coro_` prefix. It joins Step 4's spelling-only tests: 13 real failures across seven causes, 17 spelling assertions.

All twelve unfinished diagnostic sites were audited: eleven family diagnostic closures and one writeback branch. All four constructors of `LoweredCallArgument` set `writeback: false`, with no subsequent writes. The field and unreachable emission branch are removed; eleven diagnostic sites remain for Step 6.

Baseline: default **1312/1312 pass**; lowered **1282 pass, 30 fail**. Post-step gates reproduce exactly those counts. The expanded in-process corpus compares **16642 fully emitted bodies with zero stubs**. Both workspace checks, formatting, and `git diff --check` pass. The CLI corpus harness passes under both defaults (LLVM compilation, object emission, linking, and execution with the worktree standard library). Step 2 is next.

### Step 2 — parameter names (complete)

`LoweredPatternKind::Binding::name` retains the source spelling and instance cloning preserves it. `bind_pattern` applies it to ordinary LLVM binding values before storing them, just as legacy does. Singleton patterns bind nothing and remain unnamed. The four parameter-name assertions are unchanged.

P2 wording correction: legacy's skip is `resolved().type_for_pattern(...)`, which identifies a singleton pattern, not a binding's type annotation. Typed ordinary bindings are named as well; the implementation mirrors the actual legacy rule. Both workspace checks, formatting, and whitespace checks pass. Default gate: **1312/1312 pass**; lowered gate: **1286 pass, 26 fail** (four fewer). All four original parameter-name tests pass unchanged, and the expanded corpus normalized body comparison passes unchanged. Both CLI corpus harnesses pass. Step 3 starts with cause G.

### Step 3 — real failures

**G (initializer naming), complete.** Lowering assigns `LoweredInitializer::name` after catalog closure, reserving runtime, instance, artifact, and coroutine-pair symbols before allocating initializer names in stable arena order. A free legacy `__staple_init_m{prefix}` name is retained; collisions receive deterministic `.N` suffixes. The emitter reads the name, and the legacy census maps initializers through their recorded module identity. Closed-catalog validation recomputes the names; `initializer_names_are_unique_and_revalidated` tests uniqueness and rejects a corrupted name. The reduced sibling-module fixture is `MustRun` with pinned empty stdout.

The newly runnable fixture exposed a harness defect: one rename map was applied to both modules, so a planned initializer spelling that also denoted a different legacy initializer was remapped on the lowered side. The harness now uses the legacy-to-planned map only for legacy and an identity map of recorded names for lowered emission. This preserves distinct initializer identities without a new D5 explanation. The focused tests pass, and **16921 bodies compare with zero stubs**. The corrected lowered full gate has **1288 pass, 25 fail** (one fewer), across 1313 tests including the new corruption test. The corrected default gate passes **1313/1313**. Both workspace checks, formatting, whitespace checks, and both CLI corpus harnesses pass. Cause F is next.

**F (nested-expression return), complete.** Call emission previously continued after evaluating an argument that returned from the enclosing function, leaving instructions after the terminator. Call-step traversal now stops on the environment exit flag, including callee and spread evaluation; argument assembly skips pass-mode materialization after divergence. Pattern bindings also stop before binding/storing an exited initializer value. This is an emitter fix; no lowering fact is missing. The original nested-return test passes unchanged, and the reduced corpus fixture is `MustRun` with pinned empty stdout. The corpus compares **17200 bodies with zero stubs**. The default gate passes **1313/1313**; the lowered gate has **1289 pass, 24 fail** (one fewer). Both workspace checks, formatting, whitespace checks, and both CLI corpus harnesses pass. Cause E is next.

**E (whole-product trait-call slots), complete.** The failing concrete trait call had two per-slot argument records sharing one product expression but only `Argument { argument: 0 }` in its step sequence. The generic template used indirect slots; specialization recomputed both pass modes to `Value` without recomputing operand projections. `recompute_call_arguments` now replaces that shared-operand step with an explicit product spread supplying all concrete slots, evaluating the operand once. The instance validator checks that every argument record is supplied by a call step; a corruption test removes the spread and requires a missing-slot diagnostic. No emitter workaround is used. The original trait-call test passes unchanged; the reduced fixture is `MustRun` with pinned empty stdout, and **17479 bodies compare with zero stubs**. The corruption test passes. Default: **1314/1314 pass**; lowered: **1291 pass, 23 fail** (one fewer). Both workspace checks, formatting, whitespace checks, and CLI corpus harnesses pass. Cause D is next.

**D (effect-polymorphic closure resources), complete.** Effect substitution correctly rebound hidden-resource providers but rebuilt evaluation steps only for intrinsic calls. Indirect closure calls gained IO bindings without IO ABI argument steps. Every effect-aware call now rebuilds its resource steps after binding changes, in row order immediately before invocation. The instance validator checks resource-step order/count against concrete bindings, and a corruption test removes the steps from a specialized closure call. The original effect test and corruption test pass; the reduced fixture is `MustRun` and pins `hello\nhello\n`. **17758 bodies compare with zero stubs**. Default: **1315/1315 pass**; lowered: **1293 pass, 22 fail** (one fewer). Both workspace checks, formatting, whitespace checks, and CLI corpus harnesses pass. Cause C is next.

**C (natural repetition), complete.** The concrete count was substituted inside `Symbolic`, so emission still took its symbolic one-element fallback. Instance cloning now derives `Fixed(3)` (or the concrete empty/singleton shape) from the substituted uncoerced result type and recomputes `collapsed`. Template lowering and instance cloning share count classification, and both validators share count/collapse checking. The corruption test independently changes the count and collapse marker. The original test and corruption test pass; the reduced fixture is `MustRun` with pinned empty stdout. **18037 bodies compare with zero stubs**. Default: **1316/1316 pass**; lowered: **1295 pass, 21 fail** (one fewer). Both workspace checks, formatting, whitespace checks, and CLI corpus harnesses pass. Cause B is next.
