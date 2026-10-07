# Add Hybrid Polymorphism

## Summary

Make generic functions first-class values while keeping monomorphization as
the default code-generation strategy:

- **Statically known, concrete generic calls** stay monomorphized exactly as
  today.
- **Generic functions stored as polymorphic values** — in bindings, fields,
  parameters, results, explicitly typed generic containers, and mutable
  storage with a fixed scheme — use a dictionary-passed implementation.
- **Every implementation is generated only when reachable**: a generic `def`
  that is only ever called concretely produces no dictionary-passed code, and
  an unused polymorphic value produces no descriptors, dictionaries, or
  adapters.
- Independently generic trait methods and effect polymorphism are included.

```staple
def identity: <T> move T -> T = move value => value

let id = identity                       // polymorphic value
let annotated: <T> move T -> T = identity
let concrete: I32 -> I32 = identity     // instantiation: monomorphized

type Functions = wrap (
    identity: <T> move T -> T,
)

def use: (<T> move T -> T) -> (I32, String) =
    f => (f 42, f "hello")

let mut selected: <T> move T -> T = identity
```

Today the `<T>` parameters belong to the `def` binding. After this change they
are part of the function type.

A type body now needs an explicit `alias`, `wrap`, `from`, or `opaque`
marker, so the original `type Functions = (…)` example is written with
`wrap` here.

**This change is huge.** It touches every compiler phase (syntax, resolve,
typecheck, ownership, lowering/specialization, codegen ABI), the LSP, the
editor grammar, and the language reference. It also reverses two rules that
Staple.md states explicitly: "Generic functions are rank-1" and "Traits use
static dispatch … runtime dictionaries … are not currently supported". It is
split into stages below. Each stage ends with
`cargo test --workspace --quiet` passing, and the stages marked
**Breakdown plan: required** get their own `STAGE_N_*_BREAKDOWN.md` before
implementation starts.

Work starts at Stage 1. There is no separate specification stage: the
Design and Decisions sections below fix the semantics, and `Staple.md` is
updated in the same stage that makes each behavior usable, following the
repository's convention that the reference describes what the compiler
actually does.

## Current State

- **AST.**
  - `Binding` (`staple-syntax/src/ast/item.rs`) stores `type_parameters`,
    `trait_bounds`, and `subtype_bounds` beside an `annotation` that is a
    plain `Type::Function`.
  - `ResolvedFunction` (`staple-compiler/src/resolve.rs`) copies those
    fields.
  - Trait members, `impl`s, companions, type declarations, and macros carry
    their own declaration-level parameter lists.
  - A companion member's call-site parameters are the companion's parameters
    followed by the member's own. An `extern` block inside a companion
    inherits the companion's parameters and bounds.
  - Generic `extern` bindings exist, but only for the reserved
    `"staple-intrinsic"` ABI. Intrinsics must be called directly and can't be
    used as values.
  - Associated types (which replaced functional dependencies) are stored as
    hidden trailing trait parameters: `TraitDeclaration::type_parameters`
    holds the explicit parameters followed by one per associated type.
    Projections are written `Trait.Name arguments`.
  - Explicit call-site type arguments are `Expression::TypeApplication`
    (`identity.<I32>`, with `_` for an argument still to be inferred).
  - Type ascription is the prefix form `Type :: expression`.
- **Checker.**
  - There is no quantifier node. A generic function's type is a
    `CheckedFunctionType` containing free `CheckedType::Parameter` nodes.
  - `TypeChecker::instantiate_function_use` (`typecheck.rs`) separates
    generic from non-generic uses with a `generic_function: bool` flag and
    `active_generic_parameters`.
  - A generic value without a concrete expected type is rejected ("generic
    value … requires a concrete expected type").
  - `check_explicit_type_application` takes a generic value's parameter order
    from side tables, not from its type: the `call_type_parameters` map for
    functions and constructors, and `trait_parameter_arguments` for trait
    methods.
- **Lowering.**
  - Instances are keyed by `CallSubstitutions` and the
    `SubstitutionEnvironment` in `lower/instance_resolution.rs`.
  - Every lowered instance is fully concrete.
- **Codegen ABI is type-specific.** Closures are `{fn*, env*}`
  (`codegen/layout.rs`), but `compile_closure_function_type`
  (`codegen/abi.rs`) derives these from the concrete type:
  - each resource effect's pass mode (by value, or by pointer when `mut` or
    non-`Copy`);
  - each parameter's flattening and direct/indirect pass mode.

  Code that takes its types at run time therefore cannot share an ABI with
  concrete closures. Converting between the two needs adapters (Swift calls
  them reabstraction thunks) and run-time type descriptors. This is the main
  cost driver.
- **Layout.**
  - Products compile as packed structs (`compile_product_type`), so field
    offsets are running sums of field sizes.
  - Sums use a carrier sized to the largest alternative.
  - Every stack slot is allocated in the function's entry block (commit
    944cb3f).

## Design

### 1. Syntax: `Forall` is a type

- `<…>` becomes a type form:
  `Type::Forall(ForallType { parameters, trait_bounds, subtype_bounds, body })`,
  with the same parameter-list and `where`-clause grammar as today, including
  `effect` parameters and `?Sized` relaxations.
- **Only function types can be quantified.** The body must be a function type
  (directly or through an alias), so `<T> (T, T)` is rejected.
- **Scope.** A `Forall` is a prefix form whose scope extends as far right as
  possible. In parameter or element position it needs parentheses:
  `(<T> move T -> T) -> (I32, String)`.
- **The surface syntax for `def` does not change.** `def identity: <T> … = …`
  already writes `<T>` in the annotation; only the AST moves.
- `type`, `trait`, `impl`, `companion`, and `macro` keep their
  declaration-level parameters.
- **Parameter order is the scheme's order.** Explicit `.<…>` arguments follow
  the `Forall`'s parameter list. A companion member's scheme is the
  companion's parameters followed by the member's own, which keeps today's
  `call_type_parameters` order.
- **Externs.** Generic `"staple-intrinsic"` bindings get `Forall`
  annotations like any `def`. A `Forall` is rejected anywhere in an ordinary
  external ABI's signature. Intrinsics still can't be used as values, so they
  never become polymorphic values.

### 2. Scoped type variables

When a binding's annotation is a top-level `Forall`, its parameters are in
scope in the binding's value. This matches today's behavior for a generic
`def` body, including local `let`s that mention `T`.

### 3. Checking (bidirectional, predicative by default)

- **Instantiation.** A `Forall`-typed value is instantiated when it is:
  - applied (`f 42`), or
  - checked against a non-`Forall` expected type
    (`let concrete: I32 -> I32 = identity`, the existing monomorphizing path).
- **Generalization (checking against a `Forall`).** The expected scheme's
  parameters are skolemized (treated as fixed, unknown types).
  - A lambda is checked against the skolemized body.
  - A generic `def` name, trait member, or another `Forall` value is accepted
    when its scheme instantiates to the expected one: alpha-equivalence plus
    bound entailment.
  - **Subsumption is shallow** (Decision 1). Only the outermost `Forall` is
    instantiated or renamed; nested function types must match exactly.
    `(<T> T -> T) -> I32` is therefore not accepted where
    `(I32 -> I32) -> I32` is expected. Deep subsumption can be added later
    without breaking programs.
- **Explicit instantiation.** `f.<I32>` applies to a `Forall`-typed value
  exactly as it does to a generic `def`. `_` arguments are inferred. The
  result is always fully instantiated: explicit arguments never produce a
  smaller scheme.
- **Ascription supplies a scheme.** `(<T> move T -> T) :: identity` checks
  `identity` against the scheme, just as an annotated `let` does.
- **`from` types.** The implicit conversion into a `from` type never
  generalizes or instantiates. A `from` or `wrap` representation may contain
  `Forall` fields.
- **No let-generalization.** `let id = identity` without an annotation takes
  the expression's `Forall` type as-is. Unannotated lambdas are still never
  generalized.
- **Impredicativity only where written.** A `Forall` may be a type argument
  (`List (<T> T -> T)`, `Box (<T> T -> T)`) only when an annotation spells it
  out. Inference never solves a type variable to a `Forall`, so an
  unannotated `Box identity` still needs an annotation.
- **Mutable storage keeps a fixed scheme.** `selected = e` requires `e` to
  check against the stored scheme. Assigning an `I32 -> I32` is rejected.
- **Top-level `let`.** A top-level `let` may now hold a polymorphic value. It
  is an ordinary closure object built by program initialization. A top-level
  `let` whose own *type* mentions free parameters is still rejected.
- **Restrictions:**
  - no `Forall` anywhere in an ordinary `extern` signature; this is
    permanent, since C can't supply descriptors (Decision 2; generic
    intrinsics are covered in §1);
  - no `Forall`-typed signals; this can be lifted later, and `let mut` covers
    swapping the function meanwhile (Decision 2);
  - no parameter-product-capable parameters in a scheme that is used as a
    value (their arity is unknown, so no adapter can be built);
  - polymorphic recursion stays rejected (Decision 4);
  - a generic function whose body needs coroutine or reactive lowering
    can't become a polymorphic value (Decision 5). The check is transitive:
    it also covers generic callees whose dictionary-passed implementations
    the body would call with run-time type arguments. It is reported where
    the function becomes a polymorphic value, for example "functions with
    coroutine bodies cannot be used as polymorphic values yet". Direct
    monomorphized calls are unaffected.
- **Ownership is unchanged.** Ownership and trait checks already run once,
  over the generic body. A polymorphic value is a closure and has the same
  `Copy`/drop behavior as any other function value. Dictionary passing is a
  code-generation strategy only; it adds no new language rules.

### 4. Choosing monomorphization or dictionary passing

| Site | Strategy |
|---|---|
| Direct call of a known generic `def`/trait member with concrete type arguments (including inside a monomorphized instance) | Monomorphize, as today |
| A generic `def`, trait member, or scheme-annotated lambda used as a polymorphic value | Build a *polymorphic closure* pointing at its dictionary-passed implementation |
| A call through a `Forall`-typed value inside a monomorphized instance | Pass static descriptors and dictionaries |
| A call inside a dictionary-passed body whose type arguments mention run-time parameters | Call the callee's dictionary-passed implementation |
| A call inside a dictionary-passed body with only concrete type arguments | Monomorphize |
| A call through an immutable alias of a known generic `def` (`let id = identity; id 42`) | Dictionary passing in v1; devirtualized in Stage 8 (Decision 3) |

- **Implementation key.** A dictionary-passed implementation is keyed by
  `(FunctionId, concrete substitution of enclosing parameters, set of
  parameters supplied at run time)`. A generic lambda nested in a
  monomorphized `outer<I32>` therefore gets its own implementation with
  `outer`'s parameters fixed.
- **Reachability.** Dictionary-passed implementations, descriptors, static
  dictionaries, dictionary builders, and adapters are all new artifact
  families. They are requested through the existing worklist and artifact
  closure, so only reachable ones are generated.

### 5. Run-time representation

- **Type descriptors** (like Swift's value witness tables).
  - **Contents:** size, alignment, a `Copy` flag, and drop glue. A move is a
    `memcpy`.
  - Concrete types get static constants.
  - Compound types that mention run-time parameters (`(T, I32)`, `List T`,
    sums, arrays, wrappers) get descriptors built at run time.
- **Abstract ABI** for dictionary-passed implementations.
  - **Hidden arguments:** the environment, then one descriptor per type
    parameter, one dictionary per trait bound, and one *resource bundle*
    pointer per effect variable. The coroutine `CORO_RESOURCES` bundle is the
    precedent for the bundle.
  - **Values:** values typed by a run-time parameter, or containing one, pass
    by pointer.
  - **Results:** returned through an out-pointer.
  - **State effects:** erased.
- **Dynamic layout.**
  - Product offsets are running sums of field sizes, because products are
    packed.
  - Sums and arrays compute their tag position and stride from descriptors.
  - **Stack slots of dynamic size** (Decision 6). Each slot is still one
    `alloca` in the entry block, sized at run time from descriptors. All
    descriptors arrive as parameters, and types are fixed for the whole call,
    so every size is known at entry. No `stacksave`/`stackrestore` or
    mid-function allocas are needed, which keeps the spirit of the
    entry-block invariant (944cb3f). GC-backed slots are the fallback only if
    a case needing a per-iteration size turns up.
- **Adapters.** Converting a function value between its concrete and abstract
  representations needs an adapter, generated per
  `(concrete function type, abstract shape)`. Three cases:
  - a concrete `I32 -> I32` closure passed where generic code expects
    `T -> T` at `T := I32`;
  - nested function types;
  - packing and unpacking effect bundles.
- **Trait dictionaries.**
  - **Contents:** one slot per trait method (abstract ABI), dictionaries for
    prerequisite traits (including bounds declared on associated types), and
    one descriptor per associated type.
  - **Associated types.** A projection `Trait.Name T` on a run-time parameter
    resolves to the descriptor in `T`'s dictionary for that bound. A
    projection on concrete types normalizes at compile time, as it does
    today.
  - Concrete implementations get static dictionaries.
  - A generic `impl<T where Bound T>` gets a builder function that takes the
    sub-dictionaries.
  - Structural trait methods get generated slot functions.
  - A `Copy` bound uses the descriptor's flag. `Drop` is covered by the
    descriptor's drop glue, which runs any `Drop.cleanup` and then drops the
    owned fields. User code can't reference `Drop.cleanup`, so it never needs
    a dictionary slot.
- **Independently generic trait methods.**
  - A trait member's annotation may be a `Forall` over parameters beyond the
    trait's own.
  - An implementation member is checked against that scheme with the extra
    parameters skolemized.
  - Dispatch with concrete arguments monomorphizes.
  - A dictionary slot for such a member holds its dictionary-passed
    implementation, which takes extra descriptor and dictionary arguments.

## Stages

### Stage 1 — `Forall` in the syntax tree (behavior-preserving)

- Add `Type::Forall` to the AST, parser, and `Display`.
- Remove `type_parameters`, `trait_bounds`, and `subtype_bounds` from
  `Binding`. Resolve reads them from a top-level `Forall` annotation, which
  covers `def`s, generic `"staple-intrinsic"` extern bindings, and companion
  members. A companion's inherited parameters move to a separate per-member
  field instead of being merged into the member's own.
- Update resolve, typecheck, lowering, `macro_expand`, `expansion_render`,
  and the LSP to read the new shape.
- A `Forall` is accepted only at the top of a `def` or extern binding's
  annotation; anywhere else (including trait members, until Stage 7) it is a
  "not yet supported" diagnostic.
- No `Staple.md` change: the surface syntax for `def` is unchanged.
- **Exit:** identical behavior; all tests pass; emitted IR unchanged.

**Breakdown plan: required.** See
[STAGE_1_FORALL_SYNTAX_BREAKDOWN.md](STAGE_1_FORALL_SYNTAX_BREAKDOWN.md).

### Stage 2 — Explicit checked schemes (behavior-preserving)

- Add `CheckedType::Forall(CheckedScheme)` (type and effect parameters,
  bounds, body).
- Store the symbol types of generic `def`s, companion members, generic
  intrinsics, and generic constructors as schemes. `function_types` keeps
  the body templates. Trait members keep free trait parameters until
  Stage 7.
- Instantiate when a `Forall` is encountered. This replaces the
  `generic_function` flag and its `active_generic_parameters` test.
- Derive explicit `.<…>` parameter order from the scheme (companion
  parameters first, then the member's), replacing the `call_type_parameters`
  side table. Trait methods keep `trait_parameter_arguments` until Stage 7.
- Use-site bound checks read the scheme's bounds.
- `TypedModule::type_of_symbol` returns the scheme's body, so lowering,
  ownership, and the LSP are unchanged. Lowering learns about `Forall` in
  Stage 4.
- Still rank-1, still monomorphization only.
- No `Staple.md` change.
- **Exit:** identical behavior; all tests pass; emitted IR unchanged.

**Breakdown plan: required.** See
[STAGE_2_CHECKED_SCHEMES_BREAKDOWN.md](STAGE_2_CHECKED_SCHEMES_BREAKDOWN.md).

### Stage 3 — Type system for first-class polymorphism (checker only)

- Skolemization, scheme instantiation, alpha-equivalence, bound entailment,
  and effect-variable schemes.
- `Forall` in `let` and `def` annotations, product fields, wrapper payloads,
  function parameters and results, explicit type arguments, and mutable
  storage.
- Unannotated `let` carries a scheme; fixed-scheme assignment; scoped type
  variables for annotated lambdas.
- Explicit `.<…>` instantiation of `Forall`-typed values, and `Type ::`
  ascription to a scheme.
- Enforce the restrictions in Design §3 with targeted diagnostics.
- The ownership pass and LSP hover handle `Forall` values.
- Lowering rejects reachable polymorphic values with a single "not yet
  supported" diagnostic until Stage 4.
- No `Staple.md` change yet: polymorphic values can't run until Stage 4, so
  the reference is updated there.
- **Exit:** every example in the summary type-checks; negative tests cover
  each restriction.

**Breakdown plan: required.** See
[STAGE_3_FIRST_CLASS_POLYMORPHISM_BREAKDOWN.md](STAGE_3_FIRST_CLASS_POLYMORPHISM_BREAKDOWN.md).

### Stage 4 — Core dictionary passing (unbounded type parameters)

- Type descriptors: static, run-time-built, and their artifact family.
- The abstract ABI, dynamic layout for products, sums, arrays, and wrappers,
  dynamic drop glue, and descriptor-sized entry-block slots.
- Worklist keys for dictionary-passed implementations and reachability
  tracking.
- Polymorphic closure construction, calls through `Forall` values, and
  concrete↔abstract adapters, including nested function types.
- The transitive coroutine/reactive restriction from Design §3, with its
  diagnostic at the polymorphic-value site.
- **`Staple.md`:**
  - rewrite "Generic functions" around `Forall` types: polymorphic values,
    scope, instantiation, shallow scheme matching, mutable storage with a
    fixed scheme, and the restrictions in Design §3 (bounded and
    effect-polymorphic values are noted as not yet supported);
  - extend the call-site type argument rules (`.<…>`) to `Forall`-typed
    values and state that parameter order comes from the scheme;
  - update "Where a parameter product may appear" and the capable-parameter
    rules for nested schemes.
- **Exit:**
  - the summary examples compile and run;
  - reachability tests show no dictionary-passed code for concretely called
    generics and no artifacts for unused polymorphic values.

- `Copy` and `Sized` bounds, which need no dictionary (moved from Stage 6).

**Breakdown plan: required.** See
[STAGE_4_DICTIONARY_PASSING_BREAKDOWN.md](STAGE_4_DICTIONARY_PASSING_BREAKDOWN.md).
This is the largest and riskiest stage. It has two substages: 4a,
descriptors, the abstract ABI, dynamic layout, and reabstraction between
concrete and dynamic function values, tested through a hidden mode that
dictionary-passes ordinary generic calls; 4b, polymorphic closures, calls
through `Forall` values, and instantiation and scheme adapters.

### Stage 5 — Effect polymorphism at run time

- Resource bundles for effect variables in the abstract ABI.
- Bundle pack/unpack adapters at concrete↔abstract boundaries.
- `<effect E>` schemes as values, including effect-only schemes, which need
  no layout work.
- **`Staple.md`:** state that effect-polymorphic functions can be used as
  values, in the effect-parameter part of "Generic functions".
- **Exit:** `<effect E> (() ->{E} ()) ->{E} ()` works as a stored value and
  as a parameter, with resource and empty instantiations.

**Breakdown plan: not required.** Small and self-contained once Stage 4's
ABI and adapter machinery exists; it reuses the coroutine resource-bundle
pattern.

### Stage 6 — Trait bounds via dictionaries

- Dictionary layout, static dictionaries for concrete implementations, and
  run-time builders for generic implementations.
- Structural trait methods, prerequisite traits, and associated types:
  descriptors in dictionaries, and `Trait.Name T` projections on run-time
  parameters.
- `Drop` mapped onto descriptor drop glue. (`Copy` moved to Stage 4.)
- **`Staple.md`:** rewrite "Traits and bounded generic functions" to
  describe the hybrid strategy, replacing "Traits use static dispatch …
  runtime dictionaries … are not currently supported", and allow bounded
  polymorphic values in "Generic functions".
- **Exit:** bounded schemes such as `<T where Debug T> T ->{IO} ()` work as
  values, including through generic `impl`s.

**Breakdown plan: required.** It intersects implementation selection,
generic `impl` resolution, associated-type normalization, structural traits,
and the special `Copy`/`Drop` rules.

### Stage 7 — Independently generic trait methods

- Allow trait member annotations to quantify over extra parameters.
- Check implementation members against the member scheme.
- Dispatch with concrete arguments monomorphizes; dictionary slots hold
  dictionary-passed implementations; trait members can be used as
  polymorphic values.
- Explicit `.<…>` arguments on such members: trait parameters (excluding
  hidden associated-type parameters) followed by the member's own,
  replacing `trait_parameter_arguments` (Decision 7). `_` skips trait
  parameters, as in `Visitor.visit.<_, I32>`.
- **`Staple.md`:** lift the "an individual trait member may not
  independently introduce parameters" rule, document member schemes and
  implementation conformance, and add the `.<…>` order for trait members.
- **Exit:** a trait with a generic member works through static dispatch,
  through a bounded polymorphic value, and as a member value.

**Breakdown plan: required.** It changes trait declaration checking,
implementation conformance, default member bodies, explicit type arguments,
and dictionary layout together.

### Stage 8 — Extensions

- Coroutine, async, and reactive bodies in dictionary-passed
  implementations (dynamic frame slots).
- Devirtualize calls through immutable aliases of a statically known
  generic `def` (`let id = identity; id 42` is monomorphized). Never for
  `let mut`, parameters, or fields.
- Optionally, polymorphic recursion, which dictionary passing makes possible.
  It needs lowering to choose dictionary passing even in concrete contexts.
- Optionally, lift the signal restriction or add deep subsumption.

Each item updates `Staple.md` for the restriction it lifts.

**Breakdown plan: required.** The items are independent; each needs its own
small plan, and they can be scheduled separately or dropped.

## Breakdown Plans Needed

| Stage | Breakdown plan |
|---|---|
| 1 — `Forall` in the syntax tree | **Required** — [written](STAGE_1_FORALL_SYNTAX_BREAKDOWN.md) |
| 2 — Explicit checked schemes | **Required** — [written](STAGE_2_CHECKED_SCHEMES_BREAKDOWN.md) |
| 3 — First-class polymorphism in the checker | **Required** — [written](STAGE_3_FIRST_CLASS_POLYMORPHISM_BREAKDOWN.md) |
| 4 — Core dictionary passing | **Required** (split 4a/4b) — [written](STAGE_4_DICTIONARY_PASSING_BREAKDOWN.md); 4a [remainder](STAGE_4A_REMAINDER_BREAKDOWN.md), 4b [written](STAGE_4B_POLYMORPHIC_VALUES_BREAKDOWN.md) |
| 5 — Effect polymorphism at run time | Not required |
| 6 — Trait bounds via dictionaries | **Required** |
| 7 — Independently generic trait methods | **Required** |
| 8 — Extensions | **Required** (one per item) |

## Testing

- Add checker tests to `staple-compiler/tests/compiler.rs`. Negative cases
  use `.expect_err_diagnostics`, not `.expect_err`.
- Each run-time stage adds run-and-compare-output tests for the summary
  examples and its own features.
- Reachability tests inspect lowered artifacts:
  - a generic `def` called only concretely has no dictionary-passed
    implementation;
  - an unused polymorphic value produces no descriptors, dictionaries, or
    adapters.
- Stages 1 and 2 must leave the existing suite and emitted IR unchanged,
  checked with `scripts/compare-llvm-ir.py` against a **release** reference
  binary built from the stage's base commit (debug binaries take over an
  hour on the full example and corpus set). Stage 1 passed: 100 of 100
  programs `same`.
- Every stage that changes user-visible behavior updates `Staple.md` in the
  same stage.
- Suite command: `cargo test --workspace --quiet`.

## Decisions

Settled to keep the first version small. Each can be revisited later without
breaking programs written against v1.

| # | Decision | Choice | Reason |
|---|---|---|---|
| 1 | Subsumption depth | **Shallow**: alpha-equivalence, outer instantiation, and bound entailment only | Predictable errors and no checker-generated adapters; deep subsumption can be added later |
| 2 | `Forall` in signals and ordinary externs | **Rejected** | C can't supply descriptors (permanent); signals would need the reactive runtime revalidated, and `let mut` covers the use case |
| 3 | Devirtualizing calls through immutable aliases | **Deferred to Stage 8** | A pure optimization; v1 stays correct and the dictionary path stays well tested |
| 4 | Polymorphic recursion | **Rejected** | Rare, and it would break the simple rule that concrete calls are monomorphized |
| 5 | Coroutine/reactive bodies in dictionary-passed implementations | **Temporary diagnostic** (transitive, at the polymorphic-value site) until Stage 8 | Dynamic frame layout is a separate project; direct calls are unaffected |
| 6 | Stack slots of run-time size | **Entry-block allocas sized from descriptors**; GC-backed slots only as a fallback | Descriptors are parameters and types are fixed per call, so sizes are known at entry; no `stacksave`/`stackrestore` needed |
| 7 | `.<…>` order for generic trait members | **Trait parameters, then the member's own**, excluding hidden associated-type parameters; `_` skips any of them | One "outer, then inner" rule shared with companion members |
