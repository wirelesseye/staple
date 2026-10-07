# Stage 4b Breakdown: Polymorphic Values

Part of [STAGE_4_DICTIONARY_PASSING_BREAKDOWN.md](STAGE_4_DICTIONARY_PASSING_BREAKDOWN.md).
Builds on Stage 4a (`63546c5`). This plan replaces Steps 4b.1–4b.5 there.
The Stage 4 decisions (1–11) and the 4a remainder decisions (R-A to R-F)
still apply unless this plan changes them.

## Goal

Polymorphic function values compile and run. Every Goal program from
Stage 3 prints its expected output:

```staple
def identity: <T> move T -> T = move value => value

def consume: (<T> move T -> T) -> (I32, String) =
    f => (f 42, f "hello")

type Functions = wrap (identity: <T> move T -> T,)

let mut selected: <T> move T -> T = identity
let fs = Functions (identity: selected)
let pair = consume fs.identity
```

Stage 3's blanket "polymorphic function values are not supported yet"
diagnostic gives way to targeted diagnostics for the
[restrictions](#restrictions) that remain, and `Staple.md` describes
polymorphic values.

**Behavior for existing programs is preserved.** A program that uses no
polymorphic value takes no new path. Default-mode IR must match the
`50565d8` release build for every existing input, apart from the three
documented 4a fixes (`local_recursive_generic`, `local_generic_cells`, and
`dynamic_audit_forms`).

## What 4a provides

- **Dynamic instances** keyed by a run-time parameter set, with the
  dynamic signature: environment, one descriptor per run-time parameter in
  declared scheme order (R-A, enclosing parameters first), result
  out-pointer, resource pointers, then one pointer per flattened argument.
  This is already Decision 6's polymorphic ABI.
- **Descriptor-bound entries** (R-C): a closure entry that loads *every*
  descriptor from its environment and forwards to a dynamic instance.
- **Dynamic environments** (R-D): descriptors first, then captures at
  descriptor-computed offsets, with owned deep-copied descriptors and a
  dynamic finalizer.
- **Reabstraction** (R5): lowering-owned conversion plans and
  `reabstraction-thunk`/`value-conversion` artifacts between concrete and
  dynamic-shape function values, including nested functions and functions
  inside by-value aggregates.
- **The support analysis** (`lower/dynamic_support.rs`): every reason a
  template can't be emitted dynamically, transitively, classified as in
  scope or allowed. The corpus has zero in-scope rejections.
- **The differential option** (Decision 11) as a testing tool.

## Current state

Stage 4b is complete.

- **Values and calls (B1–B3).** A polymorphic value is a closure with a static
  descriptor and an alpha-canonical key. Generic `def`s, constructors, and
  scheme-checked lambdas build quantified closures in concrete and dynamic
  owners, in every storage position. A call passes one descriptor per scheme
  parameter in scheme order, packs a product instantiation of a single
  parameter, and converts function-typed arguments and results (R5) from both
  concrete and dynamic callers.
- **Adapters (B4, B5).** Using a value at a function type, or at a scheme that
  is not an alpha-renaming of its own, is an explicit coercion with one
  `instantiation-adapter` artifact. An alpha-renaming reuses the closure, and
  declared generic `def`s, constructors, and intrinsics still monomorphize.
- **Restrictions and docs (B6).** Each restriction has its own message at the
  construction site, a negative test, and a positive test of the same function
  called directly. The Generic functions section of `Staple.md` describes
  polymorphic values.
- **Audits (B7).** Reachability tests cover unused polymorphism, concrete
  instantiation, and alpha-renaming. A debug assertion compares each
  re-inferred call instantiation with the checker's type.

- **Review of `9a3dc33`.** The review found B-G unfinished and four limits
  reported to users but documented nowhere. All are resolved:
  - Instantiation adapters now apply R5 conversions per argument slot and
    for the result, with writeback for `mut` slots (B-G as planned).
  - A `mut` function argument passed to a polymorphic value from a dynamic
    body converts back after the call; this was a codegen-time error.
  - An instantiation the checker also coerces into a `from` wrapper or a sum
    composes with that coercion. The wrapper case used to fail with
    "cannot resolve type parameter `T`", and the sum case monomorphized the
    `let`-bound lambda instead of building the adapter.
  - "a polymorphic value's type parameter cannot be inferred from its use" is
    an internal invariant: the checker already rejects scheme parameters
    the body doesn't determine.
  - The one remaining limit, a single argument or result mixing the
    surrounding run-time parameters with a concrete function type, is a
    documented [restriction](#restrictions) with a test.

Verified at the review fixes:
- `cargo test --workspace --quiet`: 1,496 passed, 2 ignored.
- Full differential run (ignored by default): 119 programs identical in both
  modes; 118 use dynamic calls (13,728 call sites).
- IR against the `50565d8` release build, 4 compilations per binary, over
  132 inputs (121 corpus programs and 11 examples): every existing input is
  `same` (109); `local_recursive_generic` and `local_generic_cells` differ as
  documented 4a fixes; and 21 programs (the 20 polymorphic-value programs and
  `dynamic_audit_forms`) cannot compile on `50565d8`, as expected.

## Starting shape at the boundary (before Stage 4b)

- `TypedModule::polymorphic_value_diagnostics` (`typecheck.rs`) is Stage 3's
  gate. `Lowerer::lower` (`lower.rs`) calls it first and stops on any
  polymorphic value.
- Past the gate, `Forall` has no lowering meaning:
  - `CanonicalType` construction rejects it ("generic function types never
    reach lowering", `specialization.rs`);
  - `LayoutContext::compile_type` rejects it (`codegen/layout.rs`);
  - descriptor collection skips it (`collect_concrete_types` in
    `lower/descriptor_artifacts.rs`).
- The checker records what lowering needs, but implicitly:
  - an expression that *constructs* a polymorphic value (a generic `def`
    name or a lambda checked against a scheme) has a `Forall` expression
    type;
  - a `Forall`-typed value used at a non-`Forall` type (Stage 3 rule 2) has
    the *instantiated* function type as its expression type, while its
    symbol keeps the `Forall`;
  - a lambda checked against a scheme records it in
    `TypedModule::scheme_of_function`;
  - a call through a `Forall` value records the instantiated function type
    on the call, and lowering derives substitutions by matching a template
    against it (`call_substitutions`, `lower.rs`).
- `call_substitutions` uses `infer_type_parameters`, which refuses to bind a
  parameter to a `Forall` (Stage 3 rule 6), so an explicitly impredicative
  instantiation (`f.<(<U> U -> U)>`) needs the scheme-permitting mode.

## Decisions

- **B-A. A polymorphic value is a closure.** Its layout is the closure type
  `{fn*, env*}`, it is `Copy` (as Stage 2 made `is_copy_type(Forall)` true),
  and its drop is the closure drop. Its descriptor is the static closure
  descriptor that function types already use. `compile_type(Forall)` returns
  the closure type, and descriptor collection treats `Forall` like a
  function type.
- **B-B. Canonical keys rename scheme parameters.** `CanonicalType` gains a
  `Forall` form whose parameters are renamed positionally (the first bound
  parameter is index 0, and so on), with bounds in a canonical order. Two
  alpha-equivalent schemes then produce the same instance and artifact keys,
  which Stage 3's `types_equivalent` already treats as equal. A test checks
  that `<T> T -> T` and `<U> U -> U` give equal keys and that reordered
  parameters don't.
- **B-C. A polymorphic entry is a dynamic instance.** The entry of a
  polymorphic value is the dynamic instance of its function whose run-time
  set is the enclosing run-time parameters plus every scheme parameter. Its
  signature is already Decision 6's polymorphic ABI, so a top-level generic
  `def` or a scheme lambda in a concrete context needs no forwarding entry.
  Its closure is `{instance, env}` with a null environment or the capture
  environment.
- **B-D. Partially bound entries.** A polymorphic value built inside a
  dynamic body also needs its enclosing run-time descriptors. R-C's
  descriptor-bound entry gains a `bound: usize` count: it loads the first
  `bound` descriptors (the enclosing ones, first in R-A order) from the
  environment and forwards the remaining descriptor parameters from its own
  signature. R-C's existing use is the case where `bound` equals the whole
  run-time set. The artifact key becomes `(instance, bound)`.
- **B-E. Lambda schemes give the descriptor order.** A lambda checked
  against an expected scheme takes its `scheme_type_parameters` from
  `scheme_of_function`, in the expected scheme's order, so callers that pass
  descriptors in scheme order (B-F) match R-A.
- **B-F. Calls through a `Forall` value pass descriptors.** A call infers
  the scheme's instantiation by matching the scheme body against the call's
  instantiated function type, in the scheme-permitting inference mode. It
  then passes one descriptor per scheme parameter in scheme order (static,
  passed through from the caller's environment, or built at entry), spills
  the arguments, and reads the result from an out slot.
  - Arguments and results are converted with R5's plans wherever the caller
    side is a concrete function type and the callee side (the scheme body)
    is a dynamic-shape one, in either kind of caller.
  - No adapter is needed for the call itself.
- **B-G. Instantiation adapters.** A `Forall` value used at a non-`Forall`
  type (Stage 3 rule 2), or given `.<…>` arguments without being applied,
  becomes a closure of the target's ABI:
  - concrete ABI when the instantiated type is concrete;
  - dynamic shape when it mentions the caller's run-time parameters.

  The adapter's environment holds the polymorphic closure and deep-copied
  descriptors (R-D). Its entry passes them on, with R5 conversions per
  argument slot and for the result wherever the target side is a concrete
  function type and the scheme body's side is a dynamic-shape one; a mutated
  slot is written back after the call. The plan records those conversions as
  children, chosen by `value_conversions::adapter_conversions`, which both
  lowering and codegen use. New artifact family `instantiation-adapter`,
  keyed by `(canonical scheme, canonical target type)`. A generic `def`
  instantiated this way is still monomorphized, as today; only `Forall`
  *values* use the adapter.

  When the checker also coerces the instantiated value into a `from` wrapper
  or a sum, the instantiation is that coercion's first step: the coercion
  plan wraps or injects an `Instantiate` leaf, and the adapter produces the
  leaf's function type (`LoweredCoercionPlan::split_instantiation`).
- **B-H. Scheme adapters.** Subsumption between two schemes (Stage 3
  rule 3) needs no code when the value's parameters map one-to-one, in
  order, onto the target's: the closure is reused. Otherwise, for example
  reordered parameters or a parameter instantiated to a compound type of
  the target's parameters, an adapter entry receives the target's
  descriptors, builds the value's descriptors at entry, and forwards. Both
  sides use the dynamic shape, so no reabstraction is involved. Lowering
  recomputes the mapping by matching the value's scheme body against the
  target's with the target's parameters rigid, as B-F does. A scheme target
  is just another adapter target, so this shares B-G's
  `instantiation-adapter` family (a scheme target's key differs from every
  function target's); the separate `scheme-adapter` family first planned
  here was not needed.
- **B-I. Construction sites decide support.** Every site that constructs a
  polymorphic value (a generic `def`, constructor, or companion member used
  as a value, a lambda checked against a scheme, or an adapter) requests its
  entry's dynamic instance. The support analysis then runs for those
  instances in default mode too, not only under the differential option.
  Because there is no monomorphized fallback for a polymorphic value, every
  rejection reason becomes a diagnostic at the construction site:
  - **allowed reasons** map to the [restriction](#restrictions) messages;
  - **in-scope reasons** should be impossible after 4a's audit. They are
    reported as an internal invariant error naming the reason, not as a
    user restriction.
- **B-J. Calls stay monomorphized where they are today.** A direct call or
  concrete instantiation of a known generic `def` is monomorphized, as
  Decision 3 requires. `let id = identity; id 42` goes through the
  polymorphic value; devirtualizing it is Stage 8. Trait members can't be
  polymorphic values until Stage 7.

## Steps

Each step ends with `cargo test --workspace --quiet` passing and the quick
differential subset passing. The Stage 3 gate is narrowed step by step: it
admits exactly the forms implemented so far, so an unimplemented form never
reaches codegen. Each step adds corpus programs with `expected_stdout` for
what it enables, and each runs from a concrete context and from inside a
generic body, so the differential run executes it in both modes.

### B1 — Representation and keys

- B-A in `compile_type`, descriptor collection, the static closure
  descriptor, and ownership. B-B in `CanonicalType`, with the
  alpha-equivalence test.
- Lowering accepts `Forall` expression and symbol types without the gate
  yet; the gate stays fully in place.
- Implement the scheme-permitting inference mode in `call_substitutions`
  (rule 6's exception for written instantiations).
- **Exit:** no behavior change; the IR comparison matches.

### B2 — Constructing polymorphic values

- B-C and B-E: construction of a generic `def`, generic constructor, or
  companion member used as a value, and of a lambda checked against a
  scheme, in concrete contexts.
- B-D: the same constructions inside dynamic bodies, with partially bound
  entries.
- Storing, moving, and copying polymorphic values: bindings, `let mut`
  assignment, product and wrapper fields, function arguments and results,
  top-level `let` initializers. A polymorphic value is an ordinary closure,
  so these need only B-A.
- B-I for construction sites: the support analysis runs on demand, and its
  reasons become diagnostics.
- **Gate:** admits programs whose polymorphic values are only constructed
  and stored, never called or instantiated.

### B3 — Calls through polymorphic values

- B-F from concrete instances and from dynamic bodies, including
  descriptors built for compound instantiations (`f (1, 2)` at
  `T := (I32, I32)`) and R5 conversions for function-typed arguments and
  results.
- Explicit `.<…>` arguments applied immediately (`f.<I32> 42`), and `_`
  arguments.
- **Gate:** admits calls.
- **Corpus:** the Goal programs, plus one program per value position
  (binding, field, parameter, result, explicit type argument, `let mut`),
  each calling the value at two different types.

### B4 — Instantiation adapters

- B-G for rule 2 uses (`let c: I32 -> I32 = f` where `f` is a parameter,
  field, or `let` of `Forall` type), for unapplied `.<…>`, and for dynamic-
  shape targets inside generic bodies.
- **Gate:** admits instantiation.

### B5 — Scheme adapters

- B-H: identity subsumption reuses the closure; other subsumptions build an
  `instantiation-adapter` with a scheme target. Corpus programs cover reordered parameters
  (`<A, B> (A, B) -> A` assigned to `<Y, X> (X, Y) -> X`) and a compound
  instantiation (`<A> A -> A` used as `<T> (T, T) -> (T, T)`).
- **Gate:** the blanket gate is gone. Its message remains only as an
  internal invariant fallback, with a test that no supported program
  reaches it.

### B6 — Restrictions and `Staple.md`

- One negative test per [restriction](#restrictions), checking the exact
  message and that it points at the construction site, and one positive
  test showing the same function works when called directly.
- `Staple.md`, as listed in the Stage 4 breakdown's Step 4b.4:
  - rewrite "Generic functions" around `Forall` types, replacing "Generic
    functions are rank-1": polymorphic values, scope, instantiation,
    shallow scheme matching, mutable storage with a fixed scheme, and the
    restrictions, noting that bounds other than `Copy`/`Sized` and effect
    polymorphism are not yet supported for values;
  - extend the `.<…>` rules to `Forall`-typed values, with parameter order
    from the scheme;
  - update "Where a parameter product may appear" and the capable-parameter
    rules for nested schemes.

### B7 — Exit gate

- `cargo test --workspace --quiet` passes.
- The Goal programs and every per-position program print their expected
  output, in both modes of the full differential run.
- **Reachability**, asserted on lowered artifact families:
  - a generic `def` called only concretely has no dynamic instance;
  - a polymorphic value constructed only in an unreachable function yields
    no dynamic instance, descriptor, or adapter;
  - `let concrete: I32 -> I32 = identity` monomorphizes and builds no
    adapter;
  - an identity subsumption builds no adapter.
- **IR comparison** against the `50565d8` release build: every existing
  input `same`, apart from the three documented 4a fixes.
- The 4a coverage test still has zero in-scope rejections.

## Restrictions

Reported at the construction site (B-I), each with a test.

| Restriction | Message | Lifted in |
|---|---|---|
| Trait bound other than `Copy`/`Sized` in the scheme, or reached transitively | "polymorphic values with trait bounds are not supported yet" | Stage 6 |
| Effect variable in the scheme | "effect-polymorphic functions cannot be used as values yet" | Stage 5 |
| Coroutine, async, or reactive body, transitively | "functions with coroutine bodies cannot be used as polymorphic values yet" (and the async and reactive forms) | Stage 8 |
| `__buffer_clone`, transitively | "`__buffer_clone` cannot be used in a polymorphic value's implementation yet" | Stage 6. Unreachable from user code: `__buffer_clone` is private and its public callers need a `Clone` bound, so the trait-bound restriction applies first. Kept as an internal reason and left out of `Staple.md`. |
| Run-time `Natural` count, including `__slice_from_ref` | "array lengths cannot depend on a polymorphic value's type parameters yet" | Not planned |
| Function value behind `Ref`, `Slice`, `Buffer`, or a shared cell crossing into a polymorphic implementation | "function values inside references cannot be passed to a polymorphic value's implementation" | Not planned |
| Trait member used as a polymorphic value | "trait members cannot be used as polymorphic values yet" | Stage 7 |
| Inside a polymorphic implementation, an instantiation where one argument or the result mixes that implementation's run-time parameters with a concrete function type needing conversion, such as `(I32 -> I32, A)` for `(T -> T, U)` | "instantiating a polymorphic value at a type that combines the surrounding type parameters with a concrete function type is not supported yet" | Not planned: R5 conversions work on concrete layouts, and this needs a conversion over a run-time layout. Separate slots, such as `(I32 -> I32, A)` matched against `(T -> T, U)` as two parameters, convert normally. |

Restrictions enforced by the checker since Stage 3 (signals, ordinary
externs, capable parameters, polymorphic recursion) are unchanged.

## Intended Differences

- Programs using polymorphic values now compile and run, except where a
  restriction applies.
- Stage 3's "polymorphic function values are not supported yet" is
  replaced by the targeted messages above.
- `Staple.md` no longer says generic functions are rank-1.
- No existing program changes: the IR comparison must report `same` for
  all of them, apart from the three documented 4a fixes.

## Risks

- **Default mode now emits dynamic code.** Until now, dynamic instances
  existed only under the differential option. In 4b, any program with a
  polymorphic value gets them in default mode, so the 4a emitter's
  remaining gaps become user-visible. B-I's on-demand support analysis is
  the guard: every construction is checked before emission, and an
  in-scope reason is an internal error, not wrong code.
- **Alpha-canonical keys (B-B).** A key that distinguishes alpha-equivalent
  schemes produces duplicate artifacts (harmless). One that merges distinct
  schemes, for example by dropping parameter order or a bound, produces wrong
  code. The key test must include both cases.
- **Recomputed instantiations (B-F, B-H).** Lowering re-infers scheme
  instantiations instead of reading the checker's. If they ever disagree,
  descriptors are passed for the wrong types. A debug assertion should
  compare the re-inferred call type with the checker's recorded type at
  every polymorphic call.
- **Descriptor order (B-E).** A lambda checked against a scheme whose
  parameter order differs from the lambda's own declaration order must use
  the scheme's order. A test should use a lambda whose body mentions the
  scheme's parameters in reverse order.
- **Adapter environments.** Instantiation and scheme adapters store
  descriptors that may have been built in a caller's frame. R-D's deep copy
  must apply, and a test should call an adapter after its building frame
  has returned.
