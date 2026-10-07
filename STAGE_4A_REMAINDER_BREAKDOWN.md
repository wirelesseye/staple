# Stage 4a Remainder: Function Values, Environments, and Coverage

Part of [STAGE_4_DICTIONARY_PASSING_BREAKDOWN.md](STAGE_4_DICTIONARY_PASSING_BREAKDOWN.md).
Planned from `6a44dfa`. This plan replaces the open parts of Steps 4a.3–4a.7
there; their decisions (1–11) still apply unless this plan changes them.

## Goal

Finish Stage 4a: every generic template in the corpus is lowered through a
dynamic implementation under the differential option, unless it falls back
for a reason that belongs to a later stage. The exit gate measures this
directly, instead of only comparing output (see [R0.1](#r01--a-coverage-exit-gate)).

## Current state

**Stage 4a is complete.** R0–R8 and review problems P1–P7 are resolved.
This section is rewritten in place; validation details also live in the
conventional commits on this branch.

| Step | State |
|---|---|
| R0 — Measurement and hygiene | Done. Lowering failures fail coverage, the baseline is zero, and status is maintained in place. |
| R1 — Descriptor order | Done. Descriptors follow declared scheme order. |
| R2 — Concrete function values | Done. Concrete closure calls and boundary adapters are supported. |
| R3 — Dynamic function values | Done. Constructor adapters own their descriptors and expose the dynamic closure ABI. Shared cells that hold a function value retain the explicit Decision 7 restriction. A generic binding's initialization-state cell holds no value and is exempt. |
| R4 — Dynamic environments and captures | Done. Compound/function captures, concrete-signature adapters, native child environments, initialization bytes, and owned descriptors are supported. |
| R5 — Reabstraction | Done. Lowering-owned recursive conversions cover nested functions and by-value products, sums, wrappers, and fixed arrays normalized to products, including mutable writeback. Functions behind references retain Decision 7. |
| R6 — Buffer intrinsics and dynamic GC payloads | Done. Dynamic buffers and managed references own their descriptors and finalizers; dynamic storage loads accept packed alignment. |
| R7 — Remaining forms | Done. Concrete indexing, spreads, defaults, string patterns, propagating/asserting bindings, nested generic declarations, and shared String/CString kernels are supported. Every remaining in-scope rejection kind is audited below. |
| R8 — Exit gate | Done. Coverage, complete workspace tests, full differential execution, and default IR comparisons pass. |

**Final validation** (after the review fixes below):

- Coverage across 101 programs: 6,493 generic templates, 4,823 eligible
  (74.3%), and 1,670 rejected for allowed reasons. In-scope rejections and
  their baseline are both zero, and no template is rejected as a function
  value behind a reference.
- `cargo test --workspace --quiet`: 1,477 tests passed, zero failed, two
  intentionally ignored.
- The full differential run (ignored by default) passes all 99 runnable
  inputs with identical stdout and exit status. Dynamic calls are used by 98
  inputs, covering 11,426 call sites.
- Default IR against the `50565d8` release build, 4 compilations per
  binary for every input: 109 of 112 inputs are `same`. The three
  differences are intended bug fixes that `50565d8` miscompiles:
  - `local_recursive_generic` (new) and `local_generic_cells`: a local
    recursive generic `def` read its own state-only cell with the value-cell
    layout, out of bounds, and trapped. Only the state load changes.
  - `dynamic_audit_forms` fails to compile on `50565d8` because it
    includes the phantom-parameter case fixed in `5a5fb60`.
- Bound-free native regressions require dynamic selection for the repaired
  forms, exercise recursive conversions and mutable writeback, and assert
  alignment-one loads for packed buffer fields. The managed-reference JIT
  ownership/finalizer test checks the initialization byte and trailing
  descriptor at the current cell offsets.
- The default corpus gate checks exact exit statuses: `expect_trap`
  programs must end in the platform's `llvm.trap` signal, `expect_panic`
  programs in a Staple panic (exit status 1), and all others in 0. Expected
  stdout is pinned, including empty output.

### Review of `89de0eb`

The review that confirmed the completion claims found three problems. All
are resolved.

1. **Trap programs were not checked for the right failure.** `a0cc824` had
   relaxed the default corpus gate to accept any nonzero status for trap
   programs, so a segfault would pass. The four programs fail in two ways:
   three hit `llvm.trap`, and `dynamic_asserting_trap` panics (a failed
   `let!` exits with status 1). The corpus now distinguishes
   `expect_trap` from `expect_panic`, and the gate asserts the exact
   status of each.
2. **A local recursive generic `def` was classified as a Decision 7
   restriction.** `3acdbb9` reached zero in-scope rejections by calling its
   self-capture a function value behind a reference. But a generic binding's
   cell holds only its initialization state, never a function value, so
   nothing needs converting. The scanner now exempts captures of
   initialization-state-only symbols, and the program runs through dynamic
   implementations.

   This exposed a pre-existing default-mode bug: such a program trapped at
   run time on `50565d8` too. Whether a symbol is state-only was recomputed
   from its *substituted* type, so inside the `def`'s own instances, where
   its parameters are substituted, the self-capture was read as a value cell.
   A generic `def`'s binding now stays state-only in every instance
   (`LoweredSymbol::declared_scheme`, from
   `TypedModule::has_declared_scheme`). The `local_recursive_generic`
   corpus program covers both modes and is in the quick differential subset.
3. **An audit justification rested on a compiler bug.** The
   `ParameterDependentNonGeneric` row cited a default-emitter crash on a
   phantom-parameter local generic. Fixed in `5a5fb60`; the row was updated
   there.

### Intrinsic audit

The former `StringIntrinsic` reason is removed: parsing, UTF-8 validation,
CString bytes/NUL checks, numeric formatting, CString conversions, and string
addition have shared kernel emission. The private declarations in
`stdlib/std/string.sta`, `stdlib/std/cinterop.sta`, and
`stdlib/std/core/number.sta` are identified by owning module in
`resolve.rs`; their current callers are concrete standard-library wrappers.
A public bound-free generic caller cannot directly name those private kernels.
Their concrete behavior is exercised by the CLI string tests and full corpus.
The former catch-all `Intrinsic` and `BufferIntrinsic` reasons are removed;
the rejection classifier exhaustively lists all supported variants and only
returns allowed reasons for async/reactive operations, buffer clone, and
slice-from-reference.

### Remaining rejection-kind audit

| Reason | Evidence and scope |
|---|---|
| `CallableValue` | Bound-free callable, constructor, compound/function capture, and concrete-signature capture cases execute dynamically. Remaining runtime-shaped targets are private compiler intrinsics, private Slice constructors, foreign externs, or trait method values; their ownership/signature restrictions below prevent a public bound-free instance. Runtime-shaped shared function cells retain Decision 7. |
| `ParameterProducts` | Decision 6 excludes unknown-arity parameter products from the dynamic function ABI. `normalize_function_type` absorbs fixed parameter slots into a product/single value. Only unexpanded spreads retain `ParameterProduct`; `differential_call_runtime` refuses those signatures/substitutions. Stage 3 rejects capable parameters in schemes used as values (`typecheck.rs`), so an in-scope fixed-shape dynamic closure cannot carry one. The existing `parameter_products` corpus case exercises fixed slots. |
| `RecursiveConstruction` | Managed references are implemented and exercised. Slice's wrapper representation and constructor are private (`stdlib/std/slice.sta`; `resolve.rs` uses representation visibility for wrapper constructors); public construction routes through the explicitly restricted `Slice.from_ref` intrinsic. Syntax constructors are compile-time-only (`compile_time_only_symbol` in `lower.rs`), not runtime calls. |
| `Deferred` | The closed catalog validator reports “callable expression was not lowered” for every deferred callable. A successfully lowered runnable module cannot reach emission with one. |
| `ParameterDependentNonGeneric` | Calls with unused type substitutions retain a concrete target: `phantom_argument` in the runnable `dynamic_audit_forms` corpus entry calls a local generic `def` whose `Copy U` parameter appears nowhere in it, and runs dynamically under the differential option with identical output (its lowering regression asserts the callee keeps no relevant type parameter). The concrete emitter's former “stored closure storage” failure for this program came from call/value route classification treating a phantom-parameter `def` as non-generic, although no `<…>` binding has storage; `function_is_generic` in `lower.rs` now also consults the scheme. The guard fires only when the call's function type mentions a parameter while the target has none; a parameter actually appearing in a target signature/captures/body is collected by `relevant_parameters`, so such a target is generic. The remaining guard therefore detects inconsistent lowered records. |
| `ExternWithParameters` | `TypeChecker::check_item` rejects compile-time parameters and generic function types in foreign extern signatures. Compiler-owned generic intrinsics have a distinct target kind. A valid foreign signature therefore cannot mention a runtime parameter. |
| `Malformed` | This denotes missing or inconsistent owner-local lowered records. Closed catalog, instance graph, and artifact validation reject these before successful lowering/emission; no valid source corpus can deliberately produce one. |
| `CallsExcludedTemplate` | This is a transitive reporting label. Support and in-scope status propagate separately to a fixed point from direct leaf reasons. It can be in scope only if a reachable direct leaf is in scope; it cannot create a new independent unsupported form. |

Resolved reasons removed from the enum are covered by the bound-free dynamic
regressions: function calls/values, patterns, products/spreads/defaults,
indexing, propagation/assertions, generic bindings, and reference/buffer
operations. String intrinsic reachability and the exhaustive intrinsic
classification are documented above.

## Open problems

Found in the review of `c0cb3c5`. Each is assigned to a step and must be
resolved before R8.

- **P1. Implicit thunk arguments — resolved.** Ordinary thunks now use
  the same descriptor-bound entries and capture environments as written
  closures. The support scanner follows the thunk's own transitive reasons;
  reactive and coroutine thunks therefore remain Stage 8 fallbacks for their
  actual bodies. `dynamic_implicit_thunks` tests bound-free generic bodies,
  and a lowering assertion prevents silent fallback.
- **P2. Concrete dispatch classifications — resolved.** Concrete indexed
  mutation, slice-field mutation, and structural trait calls execute inside
  dynamic bodies. Parameter-dependent structural dispatch and indexed
  mutation use the Stage 6 `TraitCallRuntime` reason. The three blanket
  rejection reasons have been removed. `dynamic_concrete_dispatch` executes
  all three concrete forms and asserts dynamic selection for each template.
  Sum patterns obtain their payload descriptor from the payload type, so
  alternative wrappers do not pass their wrapper descriptor to a product.
- **P3. General reabstraction coverage — resolved.** The `value-conversion`
  family recursively owns its child converters and closure thunks. Bound-free
  execution and explicit dynamic-selection tests cover nested functions and
  functions in products, sums, wrappers, and fixed arrays in both directions.
  Additional execution covers opaque tuple arguments and mutable callbacks.
- **P4. The status turned back into a log.** The Stage 4 breakdown's status
  section grew again after R0.5, out of order (R4 entries after R7) and
  self-contradictory (the in-scope count both "3" and "stays 11").
  → **Fixed by this update.** Status now lives only in
  [Current state](#current-state) above, rewritten in place, and the Stage 4
  breakdown links to it.
- **P5. Skipped lowering failures — resolved.** Coverage now fails when a
  corpus program cannot lower with the differential option.
- **P6. Baseline increases — resolved.** The baseline may rise only for
  added corpus programs or corrected reason classifications, with before/after
  counts and justification in the commit message. The initial classification
  correction raised it from 3 to 4; thunk support brings the ratchet to 3.
- **P7. Dynamic `Ref` construction — resolved.** Managed references now
  use trailing descriptor words and the shared descriptor-based payload
  finalizer. The corpus constructs, reads, and replaces a `Ref T`; native
  execution checks payload preservation, exactly-once finalization, and
  recursive descriptor ownership, including zero-sized and odd-sized values.

P3 and the former P7 shared a cause: the coverage count only sees constructs the corpus
uses. Before R8, every in-scope `RejectionReason` that no corpus template
hits needs either a bound-free corpus program that exercises it or a
recorded reason why it can't occur.

## Resolved issues from the `6a44dfa` review

1. The exit gate couldn't tell finished from unfinished → R0.1 coverage gate.
2. The coverage report undercounted → R0.2 tracked test with every reason
   and transitive exclusions.
3. Descriptor order differed from Decision 6 → R1 (`e825e95`).
4. The dynamic emitter had a catch-all arm → its expression match is now
   exhaustive.
5. Two emitters, no recorded decision → R-F, recorded in the Stage 4
   breakdown.
6. The differential test was slow → a ten-program quick subset in the
   default suite and an ignored full run.
7. Two examples were missing from the differential run → `game_loop` is a
   corpus entry, and the full run covers `macros.sta` separately.
8. The status log had outgrown the plan → replaced in R0.5. It regressed
   later; see P4.
9. A test-build warning → gone.

## Baseline at `6a44dfa` (for reference)

Across the 89 corpus programs then, there were 5,698 generic templates (a
template is counted once per program that contains it). The differential
option lowered **1,527 (27%)** of them through dynamic implementations.
3,815 were rejected directly, and 356 were excluded transitively because
they called a rejected template.

Every reason that rejected at least one template, counting every reason a
template had, not only its first:

| Templates | Reason | Belongs to |
|---|---|---|
| 1,879 | callable value expression (`CallableValue`) | 4a: [R2](#r2--concrete-function-values-in-dynamic-bodies), [R3](#r3--dynamic-function-values) |
| 1,699 | value of a function type mentioning a run-time parameter | 4a: R3, [R5](#r5--reabstraction) |
| 1,698 | indirect closure call | 4a: R2, R3 |
| 1,345 | trait bound other than `Copy`/`Sized` | Stage 6 |
| 982 | trait call with run-time arguments | Stage 6 |
| 712 | intrinsics: 7 buffer operations and `__slice_from_ref` (89 each) | 4a: [R6](#r6--buffer-intrinsics-and-dynamic-gc-payloads) for six; `__buffer_clone` Stage 6, `__slice_from_ref` restriction |
| 138 | async intrinsics: completion, task, resolver (23 each) | Stage 8 |
| 189 | captures | 4a: [R4](#r4--dynamic-environments-and-captures) |
| 6 | resource effects in the signature | 4a: [R7](#r7--remaining-forms) |
| 6 | calls with resource bindings or reactive operations (one scanner reason) | 4a: R7 for resources; Stage 8 for reactive |
| 5 | reactive name reads (3), signal binding (1), `Reaction` intrinsic (1) | Stage 8 |
| 5 | string templates | 4a: R7 (concrete interpolations), Stage 6 otherwise |
| 5 | coroutine and `await` expressions | Stage 8 |
| 1 | implicit thunk argument | 4a: R7 |
| 1 | run-time repeat count | Restriction (run-time `Natural`) |

Many templates had several reasons. 182 were rejected *only* for captures
and 178 only for trait bounds.

## Decisions

These extend the Stage 4 decisions for the work left in 4a.

- **R-A. Descriptor order is the template's declaration order.** A dynamic
  instance takes one descriptor per run-time parameter, in the order the
  template declares them: scheme order for a generic `def` (companion
  parameters first), then a nested lambda's own parameters after its
  enclosing ones. Hidden projection parameters, which have ids above
  `PROJECTION_PARAMETER_BASE`, follow the declared ones. A polymorphic
  entry in 4b is then just the dynamic instance with every scheme parameter
  at run time, with no reordering adapter.
- **R-B. The ABI of a function value follows its type.** A closure is
  `{fn*, env*}` in every case. A value whose function type is concrete uses
  the concrete ABI, even inside a dynamic body. A value whose function type
  mentions a run-time parameter uses the dynamic shape from Decision 6, with
  no descriptor parameters. Indirect calls already choose the ABI from the
  callee's type, so this only needs construction to follow the same rule.
- **R-C. Descriptor-bound entries.** A dynamic-shape closure has no
  descriptor parameters, but the dynamic instance it points to does. A
  closure that refers to a dynamic instance therefore points to a generated
  *descriptor-bound entry*. That entry loads the instance's descriptors from
  the environment and forwards the call. New artifact family
  `descriptor-bound-entry`, keyed by the dynamic instance.
- **R-D. Dynamic environment layout.** An environment built in a dynamic
  body stores the descriptors its closure needs first, in R-A order and
  fixed in size, then the captures at offsets computed from their
  descriptors with the layout rule concrete environments use. Concrete
  environments are unchanged. Built descriptors are deep-copied into the
  environment, because their components may point to the building
  function's stack slots.
  - A nested lambda's key is its `FunctionId`, the enclosing concrete
    substitution, and the enclosing run-time parameters it mentions.
  - Its finalizer is a new `GcFinalizerKey::DynamicClosureEnvironment`,
    keyed by the closure instance, which drops each capture through the
    descriptors stored in the environment.
  - A shared cell holding a dynamic value is a dynamic GC payload
    (Decision 8): the cell gets a trailing descriptor word and a dynamic
    finalizer.
- **R-E. Lowering records every conversion.** Reabstraction (Decision 7) is
  decided in lowering and recorded on the boundary's argument or result as a
  conversion plan, because codegen only reads lowered records. Codegen
  emits the plans and never decides a conversion itself.
- **R-F. The dynamic emitter stays separate.** It is too large to fold into
  the concrete emitter at this point. Consistency comes from three rules:
  - semantic decisions (cleanup order, drop flags, coercions, pattern tests)
    come only from lowered records, which both emitters read;
  - both the emitter's and the support scanner's matches over lowered kinds
    are exhaustive, so a new form must be classified in both;
  - the differential run compares the two emitters' behavior on the whole
    corpus.

## Steps

Each step ends with `cargo test --workspace --quiet` passing, the quick
differential run passing (R0.4), and the coverage report showing the
listed reasons reduced. Steps R0 and R1 come first; R2–R7 are in dependency
order.

### R0 — Measurement and hygiene

#### R0.1 — A coverage exit gate

Define 4a as complete when no corpus template is rejected for an in-scope
reason. The allowed reasons are:
- trait bounds other than `Copy`/`Sized`, and trait calls with run-time
  arguments (Stage 6);
- effect variables (Stage 5);
- coroutine, async, and reactive constructs (Stage 8);
- `__buffer_clone` (Stage 6), and run-time `Natural` counts including
  `__slice_from_ref` (a restriction);
- function values behind `Ref`, `Slice`, `Buffer`, or a shared cell that
  would need converting (a restriction).

Transitive exclusions count as in scope when the callee's reason is.

A reason is allowed only for the cause the list names (P1, P2). Indexed
mutation, slice mutation, and structural trait dispatch are allowed only
when they need a trait dictionary because they involve run-time type
parameters (Stage 6). With concrete types they are in scope. Implicit thunk
arguments are allowed only when the thunk comes from a reactive construct
(Stage 8). The scanner must therefore distinguish these causes, not reject
the construct unconditionally.

#### R0.2 — A tracked coverage report

- Record every reason per template, and a separate "calls an excluded
  template" reason for transitive exclusions.
- Classify each reason as in scope or allowed, by the R0.1 list.
- Replace the ignored report with a regular test that asserts the in-scope
  count never rises above a checked-in baseline. Each step lowers the
  baseline in the same commit, and R8 sets it to zero.
- The baseline may rise only in a commit that adds corpus programs or
  reclassifies a reason, and that commit's message states the count before
  and after and why (P6).
- The test fails, instead of skipping the program, when a corpus program
  fails to lower with the option (P5).
- Keep a `--nocapture` printout of the full table for development.

#### R0.3 — Emitter consistency

- Replace the `_ =>` arm in the dynamic emitter's expression match with an
  explicit arm per unsupported kind (Issue 4).
- Record R-F in the Stage 4 breakdown (Issue 5).

#### R0.4 — Differential run time and coverage

- Split the differential test: the default suite runs a fixed,
  generic-heavy subset of about ten programs, and the full run becomes a
  separate test that the exit gate runs explicitly (Issue 6).
- Add `macros.sta` and the `game_loop` package to the corpus, or record why
  they can't be, so the full run covers every runnable example (Issue 7).

#### R0.5 — Documentation and warnings

- Replace the Stage 4 breakdown's status log with a short summary and a
  link to this plan. Validation details belong in commit messages, and
  commit references must be reachable from the branch (Issue 8).
- Remove the unused import (Issue 9).

### R1 — Descriptor order

- Implement R-A in instance keys, name planning, `compile_dynamic_function_type`,
  and every call site that passes descriptors.
- Tests: a companion member with its own parameters, a template whose ids
  are not in declaration order, and a template with a projection
  parameter, each called through the differential option.

### R2 — Concrete function values in dynamic bodies

The smallest part of the function-value work: values whose function type is
concrete (R-B), inside dynamic bodies.
- `CallableValue` for direct functions, constructors, externs, and
  intrinsic values whose type is concrete, built exactly as in concrete
  code.
- Indirect calls through concrete closures, with arguments and results
  moved between dynamic storage and the concrete ABI.
- **Measure:** callable-value and indirect-call rejections fall for
  templates whose function values are all concrete.

### R3 — Dynamic function values

- `CallableValue` of a generic `def` or constructor instantiated at
  run-time parameters builds a closure to a descriptor-bound entry (R-C).
  The environment holds the descriptors (R-D).
- Indirect calls through dynamic-shape closures: spilled arguments and an
  out slot, with no descriptor arguments.
- Function-typed values (parameters, results, product fields, sum
  payloads) in dynamic bodies are stored and moved as closures. Their drop
  is the closure drop, which is already static.
- **Measure:** the callable-value, function-typed-value, and indirect-call
  rows reach zero except where R4 or R5 is still needed.

### R4 — Dynamic environments and captures

- Nested lambdas in dynamic bodies, with R-D keys, environments, and
  finalizers, including `move`, borrowed, and shared-cell captures.
- Curried generic `def`s (`start => end => …`) are the common case, and the
  182 templates rejected only for captures should mostly be these.
- **Measure:** the captures row reaches zero.

### R5 — Reabstraction

- Decision 7 with R-E: lowering records a conversion plan for every
  argument and result at a concrete-to-dynamic call boundary whose dynamic
  type contains a function type, including nested function types and
  by-value products, sums, arrays, and wrappers.
- Artifact families `reabstraction-thunk` and `value-conversion`, keyed by
  `(concrete type, symbolic dynamic type, direction)`.
  The symbolic shape distinguishes an opaque runtime parameter from a written
  product that materializes to the same concrete type but has a different ABI.
- The restriction for function values behind references, with its
  diagnostic, reported by the support analysis as an allowed reason.
- **Measure:** no function-typed rejection remains outside the allowed
  restriction. Add corpus programs for `map`-style higher-order generics, a
  generic that returns a function, and a function inside a sum, each run in
  both modes.
- **P3 resolved:** bound-free execution and selection tests cover every shape
  in both directions, opaque tuple arguments, and mutable callback writeback.

### R6 — Buffer intrinsics and dynamic GC payloads

- Decision 8's trailing descriptor word and dynamic finalizers for buffers
  and cells. `Ref` construction gets the same treatment, though no corpus
  template needs it yet, so it gets its own execution test.
- The six remaining Decision 9 buffer intrinsics: `__buffer_with_capacity`,
  `__buffer_push`, `__buffer_pop` (which returns `Option T`, a dynamic sum),
  `__buffer_get`, `__buffer_freeze`, and `__buffer_transfer`.
- **Measure:** the only intrinsic rejections left are allowed ones.
- **Completed (P7):** dynamic `Ref` construction, differential execution,
  and native descriptor ownership/finalization tests.

### R7 — Remaining forms

- Resource effects in dynamic signatures, calls that bind resources, and
  `with` scopes, passed by pointer as Decision 6 specifies. Split the
  scanner's "resource or reactive call" reason in two so the reactive part
  becomes an allowed Stage 8 reason.
- String templates whose interpolations have concrete types. Templates
  interpolating a run-time-typed value need `Display` and become an
  allowed Stage 6 reason.
- Implicit thunk arguments.
- **Measure:** these rows reach zero or move to allowed reasons.
- **Completed (P1):** ordinary implicit thunks are supported. Their own
  transitive body restrictions decide reactive and coroutine fallbacks;
  bound-free direct and indirect regressions assert dynamic selection.

### R8 — Exit gate

- Open problems P1–P3 and P5–P7 are resolved.
- Every in-scope `RejectionReason` is exercised by a bound-free corpus
  program, or has a recorded reason why it can't occur.
- The coverage test's in-scope baseline is zero.
- The full differential run passes: identical stdout and exit status for
  every runnable corpus program and example.
- **IR comparison** without the option: every program `same` against the
  `50565d8` release build.
- `cargo test --workspace --quiet` passes, with the full differential run
  outside the default suite.
- The Stage 4 breakdown's 4a section records completion and links here.

## Risks

- **Reabstraction requires explicit conversion plans.** Bound-free cases exercise
  nested thunks and aggregate conversions; a missed conversion calls a closure with the wrong ABI, which
  crashes instead of failing to compile. R-E keeps every conversion visible
  in lowered records, and graph validation should check that every
  concrete-to-dynamic boundary with a function-typed dynamic type has a
  plan.
- **Coverage hides behind Stage 6.** Many templates are rejected for both an
  in-scope reason and a trait bound. Once the in-scope reason is fixed, the
  template still falls back, so the differential run never executes the
  fix. Each step therefore needs its own execution tests on bound-free
  programs, not only the corpus run.
- **Environment finalizers.** A dynamic environment's finalizer must read
  descriptors that live in the same allocation. If the GC finalizes
  allocations in an order that frees a descriptor's own storage first, this
  breaks. Static descriptors are global constants, so only built ones are
  at risk. R-D's deep copy keeps them inside the environment, and a test
  should finalize an environment whose builder frame is long gone.
- **Descriptor order change (R1).** It touches every dynamic call site.
  Landing it before R2 keeps the change small, while only internal callers
  exist.
