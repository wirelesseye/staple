# Stage 3 Breakdown: First-Class Polymorphism in the Checker

Part of [HYBRID_POLYMORPHISM_PLAN.md](HYBRID_POLYMORPHISM_PLAN.md). Builds on
Stage 2 (`4a58553`).

## Goal

Make generic function types first-class **in the type checker**:

```staple
def identity: <T> move T -> T = move value => value

let id = identity
let annotated: <T> move T -> T = identity
let concrete: I32 -> I32 = identity

type Functions = wrap (
    identity: <T> move T -> T,
)

def use: (<T> move T -> T) -> (I32, String) =
    f => (f 42, f "hello")

let mut selected: <T> move T -> T = identity
```

Every program above type-checks (`staple check` succeeds). Compiling one is
still rejected, by a single "not yet supported" diagnostic at the lowering
boundary, until Stage 4 adds dictionary passing.

This is the stage that settles the semantics. Its rules are Design §3 and
Decisions 1–4 of the main plan; the [Rules](#rules) section turns them into
checkable statements with a test for each.

**Behavior for existing programs is preserved.** Every program that
compiled before compiles to the same IR. New programs are accepted, and
existing errors keep their text, except where a rule below says otherwise.
`Staple.md` is not edited until Stage 4, when polymorphic values can run.

## Current Shape

- **Placement.** Stage 1's resolver check rejects a `Forall` anywhere except
  at the top of a `def` or extern annotation ("generic function types are
  not supported here yet"), and separately for trait members.
  `macro_expand` rejects `Forall` in compile-time types the same way. The
  parser rejects a top-level `Forall` on `let` and `const` ("compile-time
  parameters are only allowed on `def` bindings").
- **`Binding::generics()`** returns any top-level `Forall`, whatever the
  binding kind, and `is_generic()` drives lowering (`generic:` bindings get
  no storage global) and several checker diagnostics.
- **Schemes.** Since Stage 2, a `Forall` exists only as the stored type of a
  generic function symbol, enforced by `validate_scheme_storage` in debug
  builds. `TypedModule::type_of_symbol` returns the body, and lowering never
  sees a scheme.
- **Instantiation.** `instantiate_function_use` instantiates a `Forall` from
  an argument and/or expected type. With neither, it returns the body with
  free parameters (partial instantiation). `function_use_type` reattaches a
  known callee's scheme to a residual template. Enclosing parameters are
  rigid (`collect_rigid_parameters`).
- **Compatibility** goes through two free functions: `merge_types` (used by
  `require_compatible`) and `infer_type_parameters` (argument direction).
  `infer_type_parameters_for_expected` handles the expected-type direction.
  None of them know about `Forall` beyond Stage 2's free-variable helpers.
- **Unannotated generic values** are rejected: "generic value `x` requires a
  concrete expected type" (`check_binding`).
- **Bounds** are stored twice: in the scheme and in the side tables
  (`function_bounds`, `function_subtype_bounds`, `callable_symbol_bounds`),
  with a debug parity assertion. Stage 2 deferred removing that duplication
  to this stage.
- **Ownership** matches `CheckedType::Function` in two places. Stage 2 made
  `is_copy_type(Forall)` true, matching function types.

## Rules

Each rule gets at least one positive and one negative test in
`staple-compiler/tests/compiler.rs`, using `.expect_err_diagnostics` for the
negative cases.

1. **Where a `Forall` may be written.** Its body must be a function type,
   possibly through an alias. It may appear in:
   - `def` annotations (the def's own scheme, unchanged);
   - `let` and `let mut` annotations, as the binding's type (not as
     def-level compile-time parameters; see Step 1);
   - function parameters and results, and product and wrapper fields,
     parenthesized where Stage 1's grammar requires it;
   - explicit type arguments of type constructors (`Box (<T> T -> T)`,
     `List (<T> T -> T)`) — impredicative, but only where written;
   - type ascriptions (`(<T> move T -> T) :: identity`).

   It stays rejected in ordinary `extern` signatures (Decision 2), signal
   bindings (Decision 2), trait members (until Stage 7), `as` targets, and
   compile-time (macro) types.
2. **Instantiation.** A `Forall`-typed value is instantiated when it is
   applied, or when it is checked against a non-`Forall` expected type,
   including an inference template such as a generic callee's `A -> B`
   parameter. This keeps every existing use of a generic `def` name working
   as it does today.
3. **Generalization.** Checking a value against an expected `Forall` fixes
   the expected scheme's parameters as rigid. The value is accepted if:
   - it is a lambda that checks against the scheme's body with those
     parameters rigid and the scheme's bounds available; or
   - it has a `Forall` type (a generic `def` name, trait member, or another
     `Forall` value) whose scheme instantiates to the expected body, with
     every instantiated bound entailed by the expected scheme's bounds.

   This is **shallow** subsumption (Decision 1). Nested function types must
   match exactly after instantiation of the outermost quantifier.
4. **Equivalence.** Two schemes are equal when they are alpha-equivalent:
   the same number and kinds of parameters, bodies equal after renaming one
   scheme's parameters to the other's, and the same bounds as a set.
5. **Synthesis keeps schemes.** A generic `def` name or `Forall`-typed value
   used with no argument and no expected type has its `Forall` type.
   `let id = identity` therefore gives `id` the type
   `<T> move T -> T`, and the "requires a concrete expected type"
   diagnostic no longer applies to it. Unannotated lambdas are still never
   generalized.
6. **Predicativity of inference.** Argument-direction inference
   (`infer_type_parameters`) never binds a type parameter to a `Forall`.
   Expected-direction inference may, which is how
   `let b: Box (<T> T -> T) = Box identity` works: `T := <T> T -> T`
   comes from the annotation, and `identity` is then checked against it.
   An unannotated `Box identity` instantiates `identity` (rule 2) and keeps
   today's diagnostic when that fails.
7. **Mutable storage keeps its scheme.** `selected = e` checks `e` against
   the stored `Forall` (rule 3). An `I32 -> I32` value is rejected.
8. **Explicit arguments.** `.<…>` applies to any `Forall`-typed expression,
   in the scheme's parameter order, with `_` inferred. The result is fully
   instantiated.
9. **Bounds.** Applying or instantiating a `Forall`-typed value checks the
   scheme's bounds against the instantiation, using the bounds active at the
   use site.
10. **Effects.** `<effect E>` schemes are values like any other. Instantiation
    substitutes `E` in the body's effect rows, and the call's effects are the
    instantiated row. A lambda checked against an effect scheme treats `E`
    as a rigid row variable.
11. **Unchanged restrictions.** Polymorphic recursion stays rejected
    (Decision 4), and so do parameter-product-capable parameters in a scheme
    used as a value (Design §3).

## Steps

Each step builds and passes `cargo test --workspace --quiet` before the next
starts. From Step 2 on, each step also runs the IR comparison (release
builds, `python3 -u`, examples plus corpus) to prove existing programs are
unaffected.

### Step 1 — Syntax and resolve

- **`Binding::generics()`** returns a top-level `Forall` only for `def` and
  extern bindings. For `let`, the `Forall` is just the annotation's type, so
  `is_generic()` is false, the binding gets ordinary storage, and resolve
  treats the annotation like any other type (its `Forall` arm scopes the
  parameters).
- The parser stops rejecting a top-level `Forall` on `let`. It keeps the
  error for `const`, and for `let signal` with a new message.
- Replace Stage 1's blanket placement check with the targeted rejections
  from rule 1. Keep the trait-member message until Stage 7.
- `macro_expand` keeps rejecting `Forall` in compile-time types.
- Scoped type variables: a lambda that is the value of a `let` with a
  top-level `Forall` annotation sees that annotation's parameters, exactly
  as a `def` body does.

### Step 2 — Checked representation and the lowering gate

- `resolve_source_type_inner` maps a non-top-level `Forall` to
  `CheckedType::Forall`. It builds the parameters, resolves the bounds, and
  records trait names as Stage 2 does. A top-level `def` annotation keeps
  going through `record_function_symbol_type`.
- Type declarations accept `Forall` fields, and type constructors accept
  `Forall` arguments when written explicitly. `Sized` holds, since a scheme
  is a closure.
- Replace `validate_scheme_storage` with a well-formedness check: every
  `Forall` has parameters, a function body, and aligned bound names, wherever
  it appears.
- **Lowering gate.** At the start of `Lowerer::lower`, report every
  reachable polymorphic value with "polymorphic function values are not
  supported yet" at its source span, and stop. A polymorphic value is any
  expression, pattern, or non-def symbol whose type contains a `Forall`.
  Generic `def`s themselves are unaffected; their schemes are peeled at the
  `TypedModule` boundary as in Stage 2. `staple check` still succeeds, so
  the LSP sees the types.

### Step 3 — Equivalence and subsumption

- Add `schemes_alpha_equivalent` (rule 4), and make `merge_types` use it for
  two `Forall`s. `merge_types` never instantiates. A `Forall` meeting a
  non-`Forall` there is a mismatch, because instantiation happens earlier, in
  checking mode (rule 2).
- **Audit `==` on `CheckedType`.** Derived equality is structural, so
  alpha-variants compare unequal. List every `==`/`!=` on checked types that
  can now see a `Forall` (product and wrapper comparisons, substitution
  conflict checks in `infer_type_parameters`, refresh passes) and route
  those through a `types_equivalent` helper. This is the stage's main source
  of subtle bugs; see Risks.
- `infer_type_parameters` refuses to bind a parameter to a `Forall`
  (rule 6). `infer_type_parameters_for_expected` may bind one.
- Add `subsume(value_type, expected)` as the single entry point for
  rules 2 and 3. It instantiates a `Forall` value against a non-`Forall`
  expected type, and checks a `Forall` value against a `Forall` expected
  type by fixing the expected parameters, instantiating the value's scheme,
  matching bodies, and checking bound entailment.

### Step 4 — Checking against an expected `Forall`

- `check_expression_expected` with an expected `Forall`:
  - **Lambdas:** push the scheme's parameters as rigid
    (`active_generic_parameters`) and its bounds as active, then check the
    lambda against the body. Record the lambda's scheme in a new
    `function_schemes: HashMap<FunctionId, CheckedScheme>` on `TypedModule`,
    which Stage 4 needs for dictionary-passed implementations of generic
    lambdas.
  - **Names, accesses, and other expressions:** synthesize, then `subsume`.
- Wire the expected `Forall` through every checking-mode context: `let`
  annotations, type ascription, function arguments (`use identity`),
  product and wrapper construction (`Functions (identity: identity)`),
  returns, and assignment to `let mut` storage (rule 7).

### Step 5 — Synthesizing and using `Forall` values

- `instantiate_function_use` returns the `Forall` itself when there is no
  argument and no expected type (rule 5), instead of the body with free
  parameters. Every caller that passed such a value into a non-`Forall`
  context now goes through `subsume`, so existing programs instantiate as
  before. The IR comparison is the check that nothing changed.
- `check_binding` no longer reports "requires a concrete expected type" for
  a value whose type is a `Forall`. It keeps reporting it for leftover free
  parameters.
- Applying a `Forall`-typed value that is not a generic `def` (a parameter,
  field, `let`, or call result) instantiates its scheme from the argument
  and expected type, and checks the scheme's bounds (rule 9) and effects
  (rule 10).
- `check_explicit_type_application` accepts any expression whose type is a
  `Forall` (rule 8), not only symbols.

### Step 6 — Bounds from schemes only

- Use-site bound checks read only the scheme. The debug parity assertions
  from Stage 2 go away.
- Body checking and lowering keep the side tables, but they are now derived
  once from the schemes after signature collection and bound inheritance,
  instead of being maintained in parallel. If the derivation turns out to be
  awkward, keep both and keep the assertion, and record why.

### Step 7 — Ownership and the LSP

- The ownership pass treats a `Forall`-typed value exactly like a function
  value: the same capture, borrow, and move rules. Borrowed partial
  applications remain lexical. A `Forall` value built from a borrowed
  partial application is rejected the same way storing one is today.
- LSP hover shows schemes with trait names (Stage 2's `Display`) for
  `let id = identity`, parameters, and fields. Generic `def` hover is
  unchanged.

### Step 8 — Restrictions and diagnostics

- Each rejection in rule 1 and rule 11 gets a targeted diagnostic and a test.
- Errors involving schemes print them with `Display`, for example
  ``expected `<T> move T -> T`, found `I32 -> I32` ``.

### Step 9 — Exit gate

- `cargo test --workspace --quiet` passes.
- Every program in the Goal section, and one test per rule, type-checks
  with `staple check`. Compiling the Goal programs stops at the lowering gate
  with exactly one diagnostic per polymorphic value.
- The IR comparison over the examples and corpus reports `same` for every
  program.
- Hover shows the expected scheme text for `id`, `annotated`, `selected`,
  `f` in `use`, and the `identity` field.

## Intended Differences

- Programs from the Goal section, and anything else the rules accept, now
  type-check.
- "generic value `x` requires a concrete expected type" is no longer
  reported for a binding whose value is a generic function name or another
  `Forall` value (rule 5). Tests that pin it for that case are updated.
- Stage 1's "generic function types are not supported here yet" is
  replaced by targeted messages for the positions that stay rejected.
- **Generic bodies check bounds on their own parameters.** Use-site bound
  checks used to skip every obligation that mentioned a type parameter.
  They now skip only parameters that are still being inferred, and check
  rigid ones (the enclosing function's, or an expected scheme's) against
  the active bounds. This closes a soundness hole:
  `def outer: <A> A -> A = a => copied a` with
  `copied: <T where Copy T> T -> T` used to compile, and so did
  `outer (Token 1)` with a non-`Copy` `Token`. It is now rejected with
  "trait bound is not satisfied for `A`", and the fix is to declare
  `where Copy A`. Programs that relied on the hole stop compiling.
  Covered by `stage3_generic_bodies_check_bounds_on_their_own_parameters`.
- **Effect parameters can bind to rows with a variable.** Effect inference
  used to reject any actual row that carried an effect variable. It now
  binds the parameter to the remaining row, variable included, and rejects
  a second binding with a different variable. Rule 10 needs this to match
  `apply: <effect E> …` against an expected `<effect F> …` whose `F` is
  rigid. The IR comparison shows no existing program changed. Covered by
  `stage3_effect_parameters_bind_to_rigid_effect_rows`.

## Risks

- **Structural equality versus alpha-equivalence.** Any `==` on checked
  types missed by the Step 3 audit makes two equivalent schemes look
  different. The usual symptom is a spurious "expected `<T> …`, found
  `<U> …`". Tests should deliberately use differently named parameters for
  the same scheme in every position (fields, arguments, mutable assignment,
  type arguments).
- **Synthesis change ripple.** Returning `Forall` instead of a partially
  instantiated body (Step 5) touches every place that uses a generic `def`
  name without an argument, such as passing it to a higher-order function.
  The IR comparison and the existing suite catch regressions. Step 5 should
  land alone, after Steps 3–4, so a regression is attributable.
- **Scoped type variables for `let`.** The lambda body must use the
  annotation's own parameter ids as its rigid parameters. Fresh ids would
  make `T` in the body unrelated to `T` in the annotation.
- **`is_generic()` on `let`.** If a `let` with a `Forall` annotation is
  ever treated as a generic binding, lowering gives it no storage and the
  failure shows up far from the cause. Step 1 changes `generics()` and adds
  a test for exactly this before anything else relies on it.
- **Lowering gate coverage.** A polymorphic value the gate misses reaches
  lowering and hits Stage 2's internal invariant diagnostic instead of the
  user-facing one. Step 9 checks the gate with every Goal program and
  every rule test.

## Implementation notes

- `Forall` annotations on ordinary `let` bindings describe storage. Scoped
  quantifiers for annotated lambdas are resolved once and shared by the
  annotation and body. `TypedModule::scheme_of_function` exposes, for
  Stage 4, the scheme each function implements: a generic `def`'s or a
  scheme-annotated `let`'s written annotation, or the expected scheme a
  lambda was checked against. A lambda that fails to check against its
  expected scheme has no entry.
- Compatibility uses alpha-equivalence recursively through products,
  wrappers, containers, and function types. The equality audit also covers
  inference conflicts and fallbacks, projections and trait arguments,
  refresh passes, and structural trait checks. Primitive sentinel, symbol,
  and declaration-id comparisons remain structural.
- Expected-direction inference can solve a parameter to a written scheme.
  Result annotations supply those substitutions before call arguments are
  checked. Argument-direction inference remains predicative.
- Explicit application retains checker-only information for unresolved `_`
  arguments, so an enclosing call can infer them on arbitrary expressions.
  Its expression type remains an instantiated function, not a smaller scheme.
- The lowering gate walks initializer expressions, concrete function roots,
  and referenced generic bodies, with visited sets for recursive functions.
  It reports source diagnostics before building lowering artifacts. An
  expression is reported when its own type is a scheme, so a callee whose
  type merely mentions one (`consume` in `consume identity`) is not.
  Patterns and declarations without a value are also reported when a scheme
  is nested in their type, because a parameter such as `fs: Functions` may
  be the only place an unconstructed scheme appears. Diagnostics are
  deduplicated by span.
- The bounds side tables and parity assertions are retained. Signature
  collection, conditional implementation checking, and the subsequent bound
  inheritance pass still need their partial body templates before all schemes
  are available. Parity assertions cover declaration schemes; a stored
  lambda's scheme omits obligations already captured from its enclosing body.
  Use-site checks read schemes; body checking and lowering
  continue to use the existing side tables. Deriving these tables only once
  would require restructuring that signature/inheritance pipeline beyond
  this checker stage.
- The illustrative function named `use` is named `consume` in tests, because
  `use` is a reserved import keyword. The other Goal examples retain their
  spelling, including the `I32 -> I32` specialization.

## Validation

- Stage 3 tests cover the Goal examples, schemes in each value position,
  alpha-equivalent assignment, rigid bounds and effects, predicative inference,
  scoped lambda annotations, ownership, hover rendering, and lowering reachability.
- Release LLVM IR is unchanged for all 100 examples and code-generation corpus
  programs compared against the pre-implementation `85a4362` release build.
- `cargo test --workspace --quiet` passes on the completed implementation
  (one existing ignored test).
