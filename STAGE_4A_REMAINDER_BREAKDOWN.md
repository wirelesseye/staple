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

As of `c0cb3c5`. This section is rewritten in place at each update, not
appended to.

**Stage 4a is not complete.** The corpus is close to the exit gate, but the
gate's measurement has gaps (see [Open problems](#open-problems)). Fixing
P1 and P2 will make some hidden in-scope work visible, so the in-scope count
is expected to *rise* before it reaches zero.

| Step | State |
|---|---|
| R0 — Measurement and hygiene | Done. R0.1 and R0.2 need the amendments from P2, P5, and P6. |
| R1 — Descriptor order | Done |
| R2 — Concrete function values | Done |
| R3 — Dynamic function values | Done, except generic constructors used as dynamic-shape function values (2 templates) |
| R4 — Dynamic environments and captures | Done, including shared cells and compound capture descriptors |
| R5 — Reabstraction | Partial: direct function types in both directions only. Nested function types and functions inside products, sums, wrappers, and arrays are not converted (P3). |
| R6 — Buffer intrinsics and dynamic GC payloads | Done for buffers and shared cells. `Ref` construction is still rejected and untested (P7). |
| R7 — Remaining forms | Done for resources, `with`, and string templates. Implicit thunks were reclassified instead of implemented (P1). |
| R8 — Exit gate | Not started |

**Coverage** (`coverage_report_stays_within_baseline`): across the 93
corpus programs, 4,398 of 5,951 generic templates (74%) are lowered through
dynamic implementations. At `6a44dfa` it was 1,527 of 5,698 (27%). 1,553
templates are rejected:

| Templates | Reason | Classification |
|---|---|---|
| 1,405 | trait bound other than `Copy`/`Sized` | allowed, Stage 6 |
| 1,031 | trait call with run-time arguments | allowed, Stage 6 |
| 138 | async intrinsics | allowed, Stage 8 |
| 93 | `__buffer_clone` | allowed, Stage 6 |
| 93 | `__slice_from_ref` | allowed, restriction |
| 12 | coroutine, `await`, and reactive constructs | allowed, Stage 8 |
| 1 | run-time `Natural` count | allowed, restriction |
| 1 | implicit thunk argument | allowed, **misclassified (P1)** |
| 2 | callable value expression | **in scope** |
| 1 | value of a function type, function-typed signature (`keep`) | **in scope** |

**The three in-scope templates:**
- two generic constructors used as dynamic-shape function values (R3);
- `keep`, which needs a conversion R5 doesn't implement yet (P3).

**Verified at `c0cb3c5`:**
- **IR comparison:** without the option, all 104 corpus and example inputs
  report `same` against the `50565d8` release build.
- **Full differential run:** identical stdout and exit status on 91
  programs; 90 use dynamic calls, with 10,506 redirected call sites.
- **Coverage test:** passes at the checked-in baseline of 3.
- **Not rerun in that review:** the full workspace suite.

## Open problems

Found in the review of `c0cb3c5`. Each is assigned to a step and must be
resolved before R8.

- **P1. Implicit thunks are classified as allowed regardless of cause.**
  `d0206b8` made `ImplicitThunkArgument` an allowed Stage 8 reason because
  the only corpus case is a `reaction { … }` callback. But implicit thunking
  is a general language feature: any call passing an expression where
  `() -> T` is expected creates one (`Staple.md`, "Call-site implicit
  thunking"). A bound-free generic called as `evaluate { x }` would fall back
  without counting.
  → **R7:** support implicit thunks in dynamic bodies (they are ordinary
  closures, which R4 supports), or allow them only when the thunk comes from
  a reactive construct. Add a bound-free corpus program that thunks an
  argument inside a generic.
- **P2. Three allowed reasons were added without a plan decision.**
  `IndexedMutation`, `SliceMutation`, and `StructuralTraitDispatch` are
  allowed in `RejectionReason::in_scope`, but R0.1's list doesn't include
  them. The scanner rejects all three unconditionally, so concrete cases
  inside dynamic bodies are hidden too: indexed mutation of a concrete
  container, or structural dispatch with concrete arguments.
  → **R0.1/R0.2:** allow them only when the construct involves run-time type
  parameters (a trait call needing a dictionary, Stage 6). Classify the
  concrete cases as in scope, and record the rule in R0.1.
- **P3. The coverage gate can reach zero without general reabstraction.**
  Converting nested function types and functions inside by-value aggregates
  (Decision 7's `value-conversion` family) is unimplemented. The scanner
  rejects these cases correctly, but only one corpus template (`keep`)
  exercises them, and R5's required "function inside a sum" program was
  never added. Handling `keep` alone would bring the count to zero.
  → **R5:** implement the general conversion, and add bound-free execution
  tests for each shape in both directions: a nested function type and a
  function inside a product, a sum, a wrapper, and an array.
- **P4. The status turned back into a log.** The Stage 4 breakdown's status
  section grew again after R0.5, out of order (R4 entries after R7) and
  self-contradictory (the in-scope count both "3" and "stays 11").
  → **Fixed by this update.** Status now lives only in
  [Current state](#current-state) above, rewritten in place, and the Stage 4
  breakdown links to it.
- **P5. The coverage test skips programs that fail to lower.** It does
  `Err(_) => continue` when lowering with the option fails, which would
  lower the in-scope count for the wrong reason.
  → **R0.2:** fail the test instead.
- **P6. No rule for raising the baseline.** `b4d1c73` raised the baseline,
  because two new corpus programs brought 26 in-scope standard-library
  templates with them. The reason was sound, but the ratchet only says the
  count "may only go down".
  → **R0.2:** the baseline may rise only in a commit that adds corpus
  programs or reclassifies a reason (P1, P2). That commit's message states
  the count before and after and why.
- **P7. `Ref` construction is still unsupported, and the count can't see
  it.** The scanner still rejects recursive construction
  (`RejectionReason::RecursiveConstruction`, an in-scope reason), but no
  corpus template constructs a `Ref` of a run-time type. The count stays at
  zero for it, and R6's requested execution test was not added.
  → **R6:** implement `Ref` construction with Decision 8's trailing
  descriptor word and a dynamic finalizer, and add a bound-free corpus
  program that constructs, reads, replaces, and drops a `Ref T`.

P3 and P7 share a cause: the coverage count only sees constructs the corpus
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
  `(concrete type, direction)`.
- The restriction for function values behind references, with its
  diagnostic, reported by the support analysis as an allowed reason.
- **Measure:** no function-typed rejection remains outside the allowed
  restriction. Add corpus programs for `map`-style higher-order generics, a
  generic that returns a function, and a function inside a sum, each run in
  both modes.
- **Still open (P3):** the `value-conversion` family and nested function
  types. They need bound-free execution tests for each shape in both
  directions (a nested function type, and a function inside a product, a
  sum, a wrapper, and an array), because the corpus alone barely exercises
  them.

### R6 — Buffer intrinsics and dynamic GC payloads

- Decision 8's trailing descriptor word and dynamic finalizers for buffers
  and cells. `Ref` construction gets the same treatment, though no corpus
  template needs it yet, so it gets its own execution test.
- The six remaining Decision 9 buffer intrinsics: `__buffer_with_capacity`,
  `__buffer_push`, `__buffer_pop` (which returns `Option T`, a dynamic sum),
  `__buffer_get`, `__buffer_freeze`, and `__buffer_transfer`.
- **Measure:** the only intrinsic rejections left are allowed ones.
- **Still open (P7):** `Ref` construction and its execution test.

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
- **Still open (P1):** implicit thunks were reclassified as allowed instead
  of supported. Support them, or allow only reactive ones, and add a
  bound-free generic that thunks an argument.

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
