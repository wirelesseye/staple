# Stage 2 Breakdown: Explicit Checked Schemes

Part of [HYBRID_POLYMORPHISM_PLAN.md](HYBRID_POLYMORPHISM_PLAN.md). Builds on
Stage 1 (`c6c0339`, `b9beba9`).

## Goal

Give the checker an explicit quantifier: a generic function symbol's type
becomes `CheckedType::Forall(CheckedScheme)` instead of a function type with
free parameters. Instantiation, explicit `.<…>` arguments, and use-site bound
checks read the scheme instead of guessing genericity from free parameters.

The stage is **behavior-preserving**:

- every program compiles to the same LLVM IR;
- every rejected program is rejected with the same diagnostic text, except
  where Step 3's investigation finds a case that only an unsound heuristic
  accepted (see [Intended differences](#intended-differences));
- `Staple.md` is not edited; generic functions are still rank-1 and only
  monomorphized.

## Current Shape

- **No quantifier.** A generic `def`'s symbol type is a `CheckedFunctionType`
  mentioning free `CheckedType::Parameter` ids (and effect variables). Its
  parameter list lives in side tables:
  - `call_type_parameters: HashMap<SymbolId, Vec<CheckedType>>`, the explicit
    `.<…>` order, with effect parameters encoded through
    `effect_substitution_type`;
  - `function_bounds` / `function_subtype_bounds` keyed by `FunctionId`, and
    `callable_symbol_bounds` keyed by `SymbolId` (constructors).
- **Writers of generic symbol types** (`symbol_types.insert`):
  - constructors of generic type declarations (with
    `call_type_parameters` from the declaration's parameters);
  - `seed_binding_annotation`, for annotated bindings including generic
    intrinsics;
  - signature collection (the loop that builds `CheckedFunctionType`s and
    fills `call_type_parameters` from `ResolvedFunction::type_parameters`);
  - `ensure_function_checked`, which rewrites the symbol type after the body
    is checked.
- **Instantiation.** `instantiate_function_use(value_type, argument,
  expected, span, generic_function)` instantiates when the type contains a
  free parameter and either `generic_function` is true or some parameter is
  not in `active_generic_parameters`. Every caller passes "the callee is a
  known function symbol" (`function_symbols.contains_key`, or
  `function_origin(..).is_some()`) as `generic_function`. Unresolved
  parameters are left free when the result is still a function (partial
  instantiation).
- **Other instantiation paths.** Some callers infer directly with
  `infer_type_parameters` on a symbol's type instead of calling
  `instantiate_function_use`, for example `check_juxtaposed_method_call`.
  `typecheck.rs` has 57 `infer_type_parameters` calls in total; only those
  whose template is a generic symbol's type matter here.
- **Explicit arguments.** `check_explicit_type_application` reads
  `call_type_parameters` (functions and constructors) or
  `trait_parameter_arguments` (trait methods).
- **Rigid parameters.** `active_generic_parameters` is pushed in
  `ensure_function_checked` and also read by four other checks (unannotated
  `let` genericity, and three more sites). These uses mean "parameters fixed
  in the current body" and stay.
- **Bound inheritance.** `inherit_and_validate_generic_function_bounds`
  gives nested functions the bounds of whichever declared bounds mention
  only their free parameters. That is how a lambda inside a generic `def`
  sees `T`'s bounds. It stays, since nested lambdas are not schemes.
- **Outside the checker.** `TypedModule::type_of_symbol` and
  `declared_type_of_symbol` are read about 30 times: lowering (12),
  ownership (9), the LSP (8), specialization (1). Lowering also calls
  `infer_type_parameters` 14 times on declared-versus-instantiated types to
  rebuild call substitutions. All of these expect the free-parameter
  function type.
- **Trait methods.** `trait_method_types` holds member types with free trait
  parameters, instantiated through dispatch. They are not touched in this
  stage (Stage 7 introduces member schemes).

## Target Shape

```rust
/// A generic function type: compile-time parameters, their bounds, and the
/// function type they quantify.
pub struct CheckedScheme {
    pub parameters: Vec<CheckedSchemeParameter>,
    pub trait_bounds: Vec<CheckedTraitBound>,
    pub subtype_bounds: Vec<CheckedSubtypeBound>,
    /// Always a `CheckedType::Function`.
    pub body: Box<CheckedType>,
}

pub enum CheckedSchemeParameter {
    Type { id: TypeParameterId, name: String, sized: bool, parameter_product_capable: bool },
    Effect { id: TypeParameterId, name: String },
}

pub enum CheckedType {
    // …existing variants…
    Forall(Box<CheckedScheme>),
}
```

- `CheckedSchemeParameter::substitution_pattern()` returns today's
  `CheckedType` encoding (a `Parameter`, or `effect_substitution_type`), so
  explicit-argument and inference code keeps working unchanged.
- **Where a `Forall` may appear in this stage:** only as the value of
  `symbol_types` for a symbol with compile-time parameters, which means a
  generic `def`, companion member, generic intrinsic, or generic
  constructor. Never in `expression_types`, `pattern_types`,
  `function_types`, `trait_method_types`, or nested inside another type.
  `function_types` stays `HashMap<FunctionId, CheckedFunctionType>`: the
  body template with free parameters, which is what body checking and
  lowering need.
- **`TypedModule` boundary.**
  - `type_of_symbol` and `declared_type_of_symbol` return the scheme's body,
    so lowering, ownership, specialization, and the LSP see exactly today's
    types.
  - A new `scheme_of_symbol(symbol) -> Option<&CheckedScheme>` exposes the
    scheme for Stage 3 and later.
  - Lowering does not change in this stage. It learns about `Forall` in
    Stage 4, when polymorphic values reach it.
- **Free-variable semantics.** Helpers that look for parameters
  (`contains_type_parameter`, `type_parameter_ids`,
  `sized_type_parameter_ids`, `type_contains_parameter`) treat a scheme's
  own parameters as bound and report only free ones. `substitute_type`
  substitutes inside the body but never replaces the scheme's own
  parameters.
- **`Display`.** `<T, effect E where Bound T, T <: U> body`, matching the
  syntax `Display` from Stage 1.

## Steps

Each step builds and passes `cargo test --workspace --quiet` before the next
starts. Steps 2–5 also run the IR comparison from Step 6 on a subset (the
examples plus `calls_generic`, `templates`, `local_generic_cells`, and the
`generic_*` corpus entries), so a regression is caught in the step that
causes it.

### Step 1 — Type and helpers

- Add `CheckedScheme`, `CheckedSchemeParameter`, and `CheckedType::Forall`,
  with `PartialEq`/`Eq` (structural; alpha-equivalence is Stage 3) and
  `Display`.
- Add the `Forall` arm to every exhaustive `CheckedType` match: about 72 in
  `typecheck.rs`, 4 in `parameter_products.rs`, and about 40 across lowering,
  specialization, and codegen.
  - Checker helpers get real arms with free-variable semantics (above).
  - Lowering, specialization, and codegen arms report the existing internal
    invariant diagnostic ("generic function types never reach lowering"),
    since the boundary peels schemes.
  - `is_sized` returns true; a scheme is a function value.
- Add `TypedModule::scheme_of_symbol` and make `type_of_symbol` /
  `declared_type_of_symbol` peel a `Forall`.
- Nothing produces a `Forall` yet.

### Step 2 — Produce schemes

- Add one writer, `record_function_symbol_type(symbol, function_type,
  parameters, bounds)`, which stores a `Forall` when `parameters` is
  non-empty and a plain function type otherwise. Route all four writers
  listed under Current Shape through it. `ensure_function_checked`'s rewrite
  must keep the scheme and replace only its body.
- Fill the scheme's bounds from the same sources the side tables use today:
  `function_bounds` and `function_subtype_bounds` after inheritance and
  expansion, `callable_symbol_bounds` for constructors, and the intrinsic
  bounds from `seed_binding_annotation`. The side tables stay, because body
  checking and lowering read them.
- Add a private `symbol_value_type(symbol)` that peels a `Forall`, and use it
  at every internal `symbol_types` read except the instantiation sites,
  which also peel for now. There are 11 internal reads; most are for
  `let`-bound symbols and only need the peel to stay correct.
- Keep filling `call_type_parameters` in this step.
- **Invariant check.** At the end of `TypeChecker::check`, verify that a
  `Forall` appears only where the Target Shape allows it. Report a
  violation as an internal diagnostic in debug builds.

### Step 3 — Instantiate from the scheme

- `instantiate_function_use` takes the raw symbol type. A `Forall` is
  instantiated using the scheme's parameters as the declared set. Anything
  else is returned unchanged. Remove the `generic_function` parameter, and
  remove its `active_generic_parameters` test.
- Every caller passes the unpeeled type. The partial-instantiation rule and
  every diagnostic stay exactly as they are; with no argument and no
  expected type, the result is the scheme's body with free parameters, as
  today.
- Inventory the direct `infer_type_parameters` callers whose template comes
  from a generic symbol's type (`check_juxtaposed_method_call` and any
  others found) and make them take the scheme's body.
  `check_function_bounds(function, &raw, &instantiated)` receives the body.
- **Investigate before switching.** Today `generic_function` is true for any
  known function symbol, so a *non-generic* local `def` inside a generic body
  whose type mentions the outer `T` is also instantiated: `helper 5` with
  `helper: T -> T` infers `T := I32`. After this step it isn't instantiated,
  so the call is a type mismatch. Write tests for this case (and a local
  generic `def` inside a generic body) against the pre-change compiler
  first:
  - if the old behavior only accepts ill-typed programs, the new rejection
    is an intended difference and gets a test;
  - if a well-typed program depends on it, stop and revisit the design
    before continuing.
- Recursive calls to the generic `def` itself keep working: the scheme is
  instantiated with `T := T`, and the polymorphic-recursion diagnostic
  is unchanged.

### Step 4 — Explicit arguments from the scheme

- `check_explicit_type_application` takes the parameter list from the raw
  type's scheme via `substitution_pattern()`, for functions, companion
  members (companion parameters first, as Stage 1 merged them), generic
  intrinsics, and constructors. Trait methods keep
  `trait_parameter_arguments` until Stage 7.
- Delete `call_type_parameters`.
- The arity and "requires a generic function or constructor" diagnostics
  keep their text. A non-`Forall` raw type now produces the latter.

### Step 5 — Use-site bounds from the scheme

- `check_function_bounds` and `check_callable_symbol_bounds` read the
  scheme's bounds at use sites instead of `function_bounds` /
  `callable_symbol_bounds`. In debug builds, assert that the two sources
  agree.
- Body checking (`active_function_bounds`, bound inheritance for nested
  functions) and lowering keep reading the side tables. Removing that
  duplication is left to Stage 3, where it can be done together with
  skolemized body checking.

### Step 6 — Exit gate

- `cargo test --workspace --quiet` passes.
- Build **release** binaries of the stage's base commit and of the stage
  head, export the corpus (`STAPLE_CORPUS_DUMP=<dir> cargo test -p
  staple-compiler --lib dump_corpus_sources -- --ignored`), and run
  `scripts/compare-llvm-ir.py --runs 2` over the examples and the corpus.
  Every program reports `same`. Write results to a file with `python3 -u`
  so partial results survive.
- The invariant check from Step 2 reports nothing across the suite.
- LSP hover on a generic `def`, a companion member, a generic intrinsic, and
  a generic constructor is unchanged.

## Intended Differences

- Possibly one, decided in Step 3: a non-generic local function whose type
  mentions an enclosing generic parameter is no longer instantiated at its
  call sites. This changes only programs that the old heuristic accepted
  by inferring an outer rigid parameter as a concrete type.
- None otherwise. In particular, `Display` of a `Forall` never reaches user
  output in this stage, because diagnostics and hover print peeled types.

## Risks

- **A leaked `Forall`.** If one reaches `expression_types` or another table
  outside `symbol_types`, lowering hits the invariant diagnostic and IR
  changes. Step 2's invariant check finds this before the IR comparison
  does.
- **Two sources of bounds.** Until Stage 3, bounds live in both the scheme
  and the side tables. Step 5's debug assertion catches drift. Every write to
  `function_bounds` after signature collection (bound inheritance, in
  particular) must also update the scheme, or the scheme must be built after
  inheritance runs.
- **Ordering of writes.** `ensure_function_checked` can run before or after a
  use site reads the symbol type (recursion, forward references). Both writes
  must produce a scheme with the same parameters, so a use site sees the same
  scheme whichever write it observes.
- **Free-variable helpers.** Changing `contains_type_parameter` to ignore
  bound parameters matters only where a `Forall` is visible, which is inside
  the checker. Lowering never sees one in this stage, because of the
  boundary peel.
- **Effect parameters.** They are encoded through `effect_substitution_type`
  in substitution maps. `substitution_pattern()` must reproduce that encoding
  exactly, or explicit `.<…>` arguments for `<effect E>` functions break.
  Add an explicit-argument test for an effect-polymorphic function in Step 4.
