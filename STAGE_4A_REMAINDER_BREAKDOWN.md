# Stage 4a Remainder: Function Values, Environments, and Coverage

Part of [STAGE_4_DICTIONARY_PASSING_BREAKDOWN.md](STAGE_4_DICTIONARY_PASSING_BREAKDOWN.md).
Baseline: `6a44dfa`. This plan replaces the open parts of Steps 4a.3–4a.7
there; their decisions (1–11) still apply unless this plan changes them.

## Goal

Finish Stage 4a: every generic template in the corpus is lowered through a
dynamic implementation under the differential option, unless it falls back
for a reason that belongs to a later stage. The exit gate measures this
directly, instead of only comparing output (see [R0.1](#r01--a-coverage-exit-gate)).

## Where 4a stands

**Done** (commits `e1a88af`..`6a44dfa`):
- run-time instance keys, specialization, and validation (Step 4a.1);
- static and run-time-built descriptors, with a layout sweep against
  LLVM's `TargetData` (Step 4a.2);
- a separate dynamic emitter (`codegen/lowered/dynamic.rs`) covering 15 of
  the 22 non-async expression kinds: names, literals, blocks, products,
  repeated products, access, ascription, logical operators, `match`, loops,
  calls, strings, CStrings, mutable bindings, and assignment (Step 4a.3);
- the dynamic ABI, direct calls between dynamic and concrete instances,
  and the concrete-to-dynamic call bridge for non-function arguments
  (Step 4a.4);
- the scalar, slice, `Ref`, and buffer-metadata intrinsics (part of
  Step 4a.6);
- the hidden differential option with its transitive support analysis
  (`lower/dynamic_support.rs`), and a CLI test that runs all 86 runnable
  corpus programs both ways with identical output (9,660 redirected call
  sites).

**Coverage at the baseline.** Across the 89 corpus programs, there are
5,698 generic templates (a template is counted once per program that
contains it). The differential option lowers **1,527 (27%)** of them through
dynamic implementations. 3,815 are rejected directly, and 356 are excluded
transitively because they call a rejected template.

Every reason that rejects at least one template, counting every reason a
template has, not only its first:

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

Many templates have several reasons. 182 are rejected *only* for captures
and 178 only for trait bounds. Function values (the first three rows) are
the dominant remaining 4a item: every name that refers to a function,
constructor, extern, or intrinsic lowers to a `CallableValue`, so any
generic body that uses a function as a value is rejected today.

## Issues to fix

Found in the review of the baseline. Each one is assigned to a step below.

1. **The exit gate can't tell finished from unfinished.** Fallbacks always
   produce identical output, so "both modes match" passes at any coverage.
   → R0.1.
2. **The coverage report undercounts.** `report_dynamic_support_reasons` is
   `#[ignore]`d, records only each template's first reason, and counts
   transitively excluded templates as supported. → R0.2.
3. **Descriptor order differs from Decision 6.** Dynamic entries take
   descriptors in sorted `TypeParameterId` order (`compile_dynamic_function_type`
   in `codegen/abi.rs`). Decision 6 says scheme order for polymorphic
   entries, which 4b's callers will use. → R1.
4. **The dynamic emitter has a catch-all arm.** Its expression match
   (`codegen/lowered/dynamic.rs:3212`) ends in `_ =>`. A new lowered
   expression kind would then compile without a decision about its dynamic
   behavior. The support scanner's match is exhaustive. → R0.3.
5. **Two emitters, no recorded decision.** The dynamic emitter re-implements
   bindings, cleanup, `match`, loops, and assignment beside the concrete
   emitter. That is now the design and should be stated, with the rule that
   keeps the two consistent. → R0.3.
6. **The differential test is slow.** It takes about 705 seconds by itself,
   the risk the Stage 4 plan anticipated. → R0.4.
7. **Two examples are not in the differential run.** `macros.sta` and the
   `game_loop` package in `staple-compiler/examples` are not corpus entries,
   while the 4a gate covers every runnable example. → R0.4.
8. **The status log has outgrown the plan.** The "Implementation status"
   section of the Stage 4 breakdown is a 130-line log. It cites commits
   `8c86d8c`, `ed91238`, and `db53462`, which no branch contains (left over
   from a rewrite of history), and it lists String and CString literals as
   both done and still required. → R0.5.
9. **A test-build warning.** `dynamic_support.rs`'s test module has an
   unused `use super::*`. → R0.5.

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

#### R0.2 — A tracked coverage report

- Record every reason per template, and a separate "calls an excluded
  template" reason for transitive exclusions.
- Classify each reason as in scope or allowed, by the R0.1 list.
- Replace the ignored report with a regular test that asserts the in-scope
  count never rises above a checked-in baseline, so the number can only go
  down. Each step lowers the baseline in the same commit, and R8 sets it to
  zero.
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
  `(concrete type, direction)`.
- The restriction for function values behind references, with its
  diagnostic, reported by the support analysis as an allowed reason.
- **Measure:** no function-typed rejection remains outside the allowed
  restriction. Add corpus programs for `map`-style higher-order generics, a
  generic that returns a function, and a function inside a sum, each run in
  both modes.

### R6 — Buffer intrinsics and dynamic GC payloads

- Decision 8's trailing descriptor word and dynamic finalizers for buffers
  and cells. `Ref` construction gets the same treatment, though no corpus
  template needs it yet, so it gets its own execution test.
- The six remaining Decision 9 buffer intrinsics: `__buffer_with_capacity`,
  `__buffer_push`, `__buffer_pop` (which returns `Option T`, a dynamic sum),
  `__buffer_get`, `__buffer_freeze`, and `__buffer_transfer`.
- **Measure:** the only intrinsic rejections left are allowed ones.

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

### R8 — Exit gate

- The coverage test's in-scope baseline is zero.
- The full differential run passes: identical stdout and exit status for
  every runnable corpus program and example.
- **IR comparison** without the option: every program `same` against the
  `50565d8` release build.
- `cargo test --workspace --quiet` passes, with the full differential run
  outside the default suite.
- The Stage 4 breakdown's 4a section records completion and links here.

## Risks

- **Reabstraction is the least explored part.** Nothing exercises thunks
  yet, and a missed conversion calls a closure with the wrong ABI, which
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
