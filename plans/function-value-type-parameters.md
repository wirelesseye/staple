# Plan: Type parameters on function values

## Goal

Let a function value declare its own compile-time parameters:

```staple
def identity = <T> move x: T => T :: x
```

When the binding also has a generic function-type annotation, the two parameter
lists must be alpha-equivalent: the same count, kinds (`T` / `effect E`),
sizedness, and bounds after positional renaming. The two lists are also
independently scoped: annotation parameters are visible only in the annotation,
and value parameters only in the value.

```staple
def identity: <T> move T -> T = <U> move x: U => U :: x                    // ok
def copy_identity: <T where Copy T> T -> T = <U where Copy U> x: U => x   // ok

def identity: <T> move T -> T = move x => x                 // error: value declares no parameters
def identity: <T> move T -> T = <U> move x: T => T :: x     // error: `T` not in scope in the value
def copy_identity: <T where Copy T> T -> T = <U> move x: U => U :: x   // error: bounds differ
```

The idiomatic style becomes value-only definitions, so stdlib, examples, docs and
tests get migrated:

```staple
def foo: <T, U> T ->{IO} U = x => ...      // before
def foo = <T, U> x: T =>{IO} U :: ...      // after
```

The same rule applies to impl members that implement a generic trait member.
They must restate the trait member's own parameters:

```staple
trait Visit T { visit: <A> (T, move A) -> A }
impl Visit I32 { visit = <B> (_, move value: B) => value }   // ok
impl Visit I32 { visit = (_, move value) => value }          // error: value declares no parameters
```

## Where generics live today (the key constraint)

Generics currently exist only as `Type::Forall` on the binding annotation:

- `Binding::generics()` / `all_type_parameters()` / `all_trait_bounds()` /
  `all_subtype_bounds()` / `is_generic()` (`staple-syntax/src/ast/item.rs:482-528`)
  read `annotation` only. About 25 call sites in the resolver, typechecker,
  lowering and LSP rely on them.
- The resolver declares the annotation's parameters in a type-parameter scope
  that **also covers the value** (`resolve.rs` ~3963-4044). That is why bodies
  such as `capture_member` in
  `tests/fixtures/generic_trait_members/values.sta` can name `T` today. The new
  rules forbid this.
- The typechecker builds the function's `CheckedScheme` from
  `ResolvedFunction.binding_annotation` (`seed_function_types`, `typecheck.rs`
  ~5093). It checks lambdas against an expected `Forall` in
  `check_expression_expected` (~8426), which pushes `active_generic_parameters`,
  bounds and `function_schemes`.
- `schemes_alpha_equivalent` (`typecheck.rs` ~16177) already implements
  positional renaming with kind, sizedness and bound-set comparison. Reuse it.
- The parser explicitly rejects `def name<T>` and suggests writing generics on
  the annotation (`parser.rs` ~1695).

**Design decision:** the *value's* parameter list becomes the canonical generic
signature of the binding. Its `TypeParameterId`s are what the body, schemes,
monomorphization and LSP use. An annotation `Forall` becomes a contract that is
resolved in its own isolated scope, renamed onto the value's ids, and then used
as the expected type.

## Implementation steps

### 1. AST (`staple-syntax/src/ast/expression.rs`, `ast/item.rs`)

- Add `pub generics: Option<GenericParameters>` to `FunctionExpression`. It comes
  before `pattern` in source order. Document that it is only meaningful on the
  outermost function of a curried chain.
- Change `Binding::generics()` to prefer the value's generics:
  1. `value` is `Expression::Function(f)` with `f.generics = Some(g)` → `g`
  2. otherwise the current annotation `Forall` behaviour (needed for `extern`
     members and for the "annotation without value generics" error path)
- Keep `annotation_body()` unchanged. Add `annotation_generics()`, returning the
  annotation's `Forall` generics, for the equivalence check and LSP.
- Make `Binding::generics()` apply to `let` bindings too when the value carries
  generics. Today the `let` + `Forall` case is special-cased in the resolver.
- Fix every `FunctionExpression { .. }` construction to add `generics: None`:
  the parser, `lower.rs` tests (~18301), and macro-generated functions in
  `macro_expand.rs`.

### 2. Parser (`staple-syntax/src/parser.rs`)

- In `parse_function_expression` (~1969): if `self.at_operator("<")`, call
  `parse_bracketed_generics()` and wrap the result in `GenericParameters`, the
  same way `parse_type` does at ~2309. `parse_bracketed_generics` already
  rejects product parameters and parses `where` clauses.
- `parse_expression` (~1935) first tries `parse_type() + ::`. A leading `<T> x: T
  => …` parses as the type `<T> x` and then fails to find `::`, so it falls
  through correctly. Add a test so this stays true. When the expression starts
  with `<` and the function parse fails, return the function error instead of
  falling back to `parse_pipe_expression`, because `<` cannot start any other
  expression.
- Only the outermost function of a chain may carry generics. Reject
  `x => <T> y: T => …` with "compile-time parameters must be on the outermost
  function value". See "Scope decisions" below for contextual `Forall`
  positions, which stay allowed.
- Update the `def name<T>` error text (~1695) to suggest the new style:
  ``write `name = <T> x: T => …` ``.
- Allow a generic *function value* in `signal`/`const` positions only where a
  generic annotation is already allowed. Mirror the existing checks at
  ~1710-1715 for value generics: a `const` value with generics gets
  "compile-time parameters are only allowed on `def` bindings".

### 3. Formatter (`staple-syntax/src/formatter.rs`)

- **Existing bug:** `staple fmt` currently turns `<T where Copy T> T -> T` into
  `< T where Copy T >`. Verified with `target/debug/staple fmt -`. The migrated
  stdlib will put many more `<…>` lists through the formatter, so fix this
  first. In the token-based formatter, treat `<` as a generic opener and `>` as
  its closer when the `<` begins a type or function value: after `:`, `=`, `(`,
  `,`, `::`, a line start, or `=>`/`->`. Comparison operators keep their spaces
  (`x < y`).
- Test idempotence for `def f = <T where Copy T> x: T => x`, curried chains, and
  `<effect E>`.

### 4. Name resolution (`staple-compiler/src/resolve.rs`)

- `resolve_binding` (~3955-4044):
  - Declare the **annotation** `Forall` in its own scope, using the existing
    `resolve_forall_type` pattern (push scope, declare, resolve body and bounds,
    pop), so annotation parameters never reach the value.
  - Keep companion generics (`binding.companion_generics`, e.g. `T` in
    `impl Slice T { def from_ref = <N where Natural N> … }`) in the outer scope
    around both annotation and value. Only the member's *own* annotation
    parameters stop leaking.
  - Store `binding_type_parameters` / `binding_trait_bounds` /
    `binding_subtype_bounds` from the value's generics when present, and fall
    back to the annotation's otherwise.
- `resolve_expression` for `Expression::Function` (~4655): when
  `function.generics` is `Some`, push a type-parameter scope, declare each
  parameter (`declare_type_parameter_pattern`), resolve trait bounds and
  subtype bounds the same way the binding path does, then resolve the pattern,
  effect lower bound and body inside that scope, and pop it.
- Add `pub generics: Option<GenericParameters>` to `ResolvedFunction` (~386) and
  fill it where `effect_lower_bound` is filled (~4708).
- Diagnostic for rejected case 2: when a type name fails to resolve inside a
  function value and matches a parameter of the enclosing binding's annotation
  `Forall`, add a hint: "`T` is a compile-time parameter of the type annotation,
  which is not in scope in the function value; use `U`, the value's own
  parameter". Keep the annotation's parameter names in a side stack for this.
- Mirror the scoping in `resolve_compile_time_expression_annotations` (~4106),
  the lenient path, and in `resolve_macro_declaration`'s walk over function
  chains.

### 5. Type checking (`staple-compiler/src/typecheck.rs`)

- **Shared helper:** pull out the part of `resolve_source_type`'s `Type::Forall`
  arm that turns `GenericParameters` into `(parameters, trait_bounds,
  trait_bound_names, subtype_bounds)`, so the value's generics can produce a
  `CheckedScheme` without a `Type::Forall`.
- **Shared helper:** split `schemes_alpha_equivalent` into
  `alpha_renaming(actual_params, expected_params) -> Option<HashMap<TypeParameterId, CheckedType>>`
  plus the bound comparison. Use it to *rename* the annotation scheme onto the
  value's ids, not just to compare. The existing callers (~13707, ~13938,
  ~16085, ~16303) keep their behaviour.
- `seed_function_types` (~5093). For a function with `generics`:
  - **With an annotation `Forall`:** resolve both parameter lists and check
    count, kinds and sizedness positionally, then check bounds as sets after
    renaming. On success, substitute the annotation body onto the value's ids
    and use the result as the function's scheme, so the existing paths read
    parameter, result and effects from it. On failure, emit a diagnostic at the
    value's `<…>` that names the first mismatch, for example "the function type
    requires `Copy T`, but the function value's `U` has no matching bound", and
    then fall back to the value's own generics to avoid cascading errors.
  - **With an annotation that is a plain function type:** error, "the function
    type has no compile-time parameters, but the function value declares `<T>`".
  - **Without an annotation:** build the scheme from the value's generics. The
    parameter type comes from the pattern, the result from `result_annotation`
    or inference, and effects from `effect_lower_bound` merged with inferred
    effects (existing machinery). Every pattern binding needs a type that is
    fully known in terms of the declared parameters. The checker does not infer
    generalization, so `<T> x => x` gets "parameter `x` needs a type annotation
    in a generic function value".
- For a function **without** `generics` but with an annotation `Forall` and a
  function-expression value (rejected case 1): error at the value's pattern,
  "the function type declares compile-time parameters `<T>`, but the function
  value declares none; write `<T> move x: T => …`". Place this check so it
  covers binding annotations, impl members, and the contextual-`Forall` path in
  `check_expression_expected` (~8426). See scope decision 1.
- **Impl members of generic trait members.** Today the expected type comes from
  `trait_method_types` substituted with the impl's trait arguments
  (~3743-3755), and the scheme from `trait_member_schemes[method]` (the
  `Type::Forall` branch). With value generics, apply the same "rename onto the
  value's ids" step as for binding annotations. Start from the trait member's
  scheme after impl substitution. Only the member's *own* parameters (e.g.
  `<A>`) are matched against the value's `<…>`. Trait parameters and `impl <T>`
  parameters stay in the outer scope, just like companion generics. Insert the
  renamed scheme into `function_schemes` and `impl_function_types`, so the
  existing checks for `mut`/`move` markers (~5120) and effects (~6203) run
  unchanged. Error spans point at the impl member's value. A mismatch message
  names the trait member, e.g. "`Visit.visit` declares `<A>`, but this
  implementation declares no compile-time parameters".
- Default methods in a trait body (`default_methods`, ~3413) follow the same
  rule when their value is a function expression.
- Body checking: with value generics, push `active_generic_parameters`,
  `active_function_bounds`, `active_subtype_bounds` and
  `function_declared_bounds` the same way `check_expression_expected` does for
  an expected `Forall`. Share that block as a helper and do not copy it.
- The binding's symbol type becomes `CheckedType::Forall(scheme)`. In
  `check_binding` (~8020) the `polymorphic_storage_symbols` /
  `binding.is_generic()` logic works unchanged because `is_generic()` now sees
  value generics.
- **Inferred results:** an unannotated generic `def` may leave its result
  inferred. The scheme body is completed after the body is checked, using the
  `checking_bindings` on-demand path at ~7930. A recursive reference made
  before the result is known emits "a recursive generic function value must
  ascribe its result type (`R :: …`)". This keeps `def drop = <T> move value:
  T => ()` valid without writing `() :: ()`.

### 6. Lowering and specialization (`lower.rs`, `specialization.rs`, `lower/*`)

- These layers use `Binding::generics()`/`is_generic()` and the
  resolver's `binding_type_parameters`, so they follow the canonical-ids change
  automatically. Audit the 3 `lower.rs` call sites and the instantiation
  substitution keyed by `TypeParameterId`. Check that substitutions are built
  from the value's ids, not from ids in a now-isolated annotation.
- Add `generics: None` in test fixtures that build `FunctionExpression`.

### 7. Macros (`macro_expand.rs`, `expansion_render.rs`)

- `freshen_expression`: freshen `generics` syntax and parameter names, and
  freshen bound/argument types (`freshen_type`), the same way
  `effect_lower_bound` is handled (~7078).
- `substitute_splices`: substitute inside bound arguments, and support a
  `TypeParameterPattern::Splice` (`$T...`) inside value generics.
- `render_expr` for `Expression::Function`: print `{generics} ` before the
  pattern using the existing `Display for GenericParameters`.
- Quote templates (`resolve_quoted_expression`) need to scope value generics so
  that `quote { <T> x: T => x }` works.

### 8. LSP (`staple-cli/src/lsp/*`)

Follow the pattern of commit `9a2fec9`, which added `effect_lower_bound`
handling to each collector:

- `semantic.rs`: classify value-generic names as type parameters and visit bound
  traits and arguments.
- `definition.rs`: index declarations of value generics, so go-to-definition on
  `T` inside `x: T` jumps to `<T>`, plus bound trait references.
- `hover.rs`: hovering a value generic shows its bounds. The binding hover still
  shows `def identity: <T> move T -> T`, derived from the scheme. Add a test
  for the value-only form.
- `completion.rs`: offer value generics inside the value. Stop offering
  annotation parameters inside the value body.
- `signature.rs`: verify that signature help for calls to value-only generic
  defs shows the scheme.
- `editors/vscode/syntaxes/staple.tmLanguage.json`: highlight `<…>` after `=`
  and in function-value position if the grammar currently keys on `:`.

### 9. Language reference (`Staple.md`)

- Rewrite the generic functions section around the value form. Document:
  value-only definitions; independent scoping; alpha-equivalence rules
  (positional, kinds, sizedness, bounds as sets); requirement for parameter
  types; result inference and the recursion caveat; generics only on the
  outermost function value.
- Migrate the ~21 generic `def` examples and the ~54 annotated `def` examples
  to the value style where they define functions. Keep a short "with an
  explicit type annotation" subsection that shows the alpha-equivalent form.
- Update the effect lower-bound section (~1550) so its example uses the
  combined form `<effect E> f: (() ->{E} ()) =>{E} f ()`.

## Scope decisions

1. **Where the "value must restate the parameters" rule applies (confirmed).**
   It applies to any function expression checked against an expected `Forall`:
   binding annotations, **impl members of generic trait members** (`impl Visit
   I32 { visit = <B> (_, move value: B) => value }`) and trait default methods,
   `Type :: value` ascriptions, and arguments to higher-rank parameters (e.g.
   `consume` in `generic_trait_members/values.sta`). One uniform rule also lets
   impl bodies name their own parameters, which they cannot do today.
2. **Non-function values with a `Forall` annotation** (`let f: <T> T -> T =
   identity`): unchanged. The rule only targets function-expression values.
   `def` already requires a function body for generics (`typecheck.rs` ~8023).
3. **Monomorphic annotated defs** (`def f: I32 -> I32 = x => …`, 81 in stdlib):
   stay legal. Migrate them to `def f = x: I32 => I32 :: …` in a separate
   style-only commit to keep the semantic diff reviewable.

## Migration

Measured counts of `def name: <…>` / any annotated `def`:

| Location | generic | annotated |
|---|---|---|
| `stdlib/**/*.sta` | 13 | 81 |
| `staple-compiler/examples` | 4 | 26 |
| `staple-compiler/tests/fixtures` | 23 | 37 |
| `Staple.md` | 21 | 54 |
| Rust-embedded sources: `tests/compiler.rs` | 208 lines | — |
| `src/lower.rs` (tests) | 62 | — |
| `tests/modules.rs`, `staple-syntax/tests/parser.rs`, LSP tests | 8 / 14 / 14 | — |

Approach:

1. Write a throwaway script under `scripts/` (or this plan's scratch space; do
   not commit it) that rewrites the common shape
   `def N: <G> P₁ -> … -> Pₙ ->{E} R = p₁ => … => pₙ => body` into
   `def N = <G> p₁: P₁ => … => pₙ: Pₙ =>{E} R :: body`. It moves `move`/`mut`
   markers onto the pattern side, which is already required. For product
   parameters `(A, B) -> R = (a, b) =>` it produces `(a: A, b: B) =>`. Where
   annotation and value bindings do not line up one-to-one (destructuring
   patterns, slotted products, `;` parameters, multi-line bodies), print the
   location for manual migration instead of guessing.
2. Run `cargo test` and `staple fmt` across stdlib and examples. Errors from
   rule 1 and rule 2 diagnostics point exactly at the sites the script missed.
3. Hand-migrate the stdlib. Notable cases:
   - `core/range.sta` curried generics:
     `def range = <T where Copy T> start: T => end: T => Range T :: Range (…)`
   - `slice.sta` companion member `from_ref`: keep companion `T`, move only `<N
     where Natural N>`.
   - `coroutine.sta` `completion`, which has a multi-line body and a labelled
     product result.
   - `core/drop.sta`: `def drop = <T> move value: T => ()`
4. Fixtures where bodies currently name annotation parameters (e.g.
   `capture_member` uses `T` in `let f: (T, …)`) must move to the value form,
   or the resolver now reports an unknown type.
5. Migrate impl members and default methods of generic trait members. These
   sites are in `generic_trait_members/*.sta` (`Visit.visit`, `Action.act`,
   including the `impl <T> … (Box T)` variants), stdlib impls, and the
   compiler tests. For example, `impl Action I32 { act = <effect E> (_, f: ()
   ->{E} I32) =>{E} f () }`. The script in step 1 cannot see trait
   declarations, so find these sites with the new mismatch diagnostic.

## Tests

- **Parser** (`staple-syntax/tests/parser.rs`): value generics with and without
  `where`; `<effect E>`; curried chains; `=>{E}` combined with generics; `<T>`
  rejected on an inner function; `x < y` still parses as comparison; no
  ascription mis-parse of `<T> x: T => T :: x`; lossless `syntax.text()`
  round-trip.
- **Formatter:** the generic-bracket spacing fix and idempotence.
- **Compiler** (`tests/compiler.rs`): one test per accepted example in the goal,
  and each rejected example asserting its diagnostic message and span. Also
  cover: count, kind and sizedness mismatch; extra bound on the value; renamed
  multiple parameters `<T, U>` vs `<A, B>`; mismatched `move` marker caught by
  body equivalence; value generics against a non-generic annotation; missing
  parameter type in an unannotated generic value; recursive generic with and
  without result ascription; explicit type application order following the
  value's list; run-time output of a monomorphized value-only generic.
- **Impl members:** an impl member with renamed parameters (`<B>` for trait
  `<A>`) is accepted. Rejected: no parameters, the wrong number of parameters,
  `effect` vs type kind mismatch, and the trait member's parameter name used in
  the impl body when the impl declares a different name. Also cover: `impl <T>
  Visit (Box T)`, where the impl's `T` stays visible and only `<A>` is matched;
  a trait default method; dynamic and dictionary dispatch through
  `generic_trait_members/{values,dictionaries,static}.sta` producing unchanged
  run-time output after migration.
- **Modules:** calling a `pub def f = <T> …` across modules, and companion
  members with value generics.
- **LSP:** semantic tokens, hover, definition and completion for value
  generics, matching the tests added in `9a2fec9`.
- **Regression:** `scripts/compare-run-output.py` and
  `scripts/compare-llvm-ir.py` across examples before and after migration.
  Generated IR should be identical, since only surface syntax changed.

## Suggested commit sequence

1. `fix: format generic parameter brackets without inner spaces`
2. `feat: parse type parameters on function values` (AST, parser, formatter, macros, render)
3. `feat!: scope type parameters to function values` (resolver, typechecker, lowering audit, LSP, compiler tests)
4. `refactor!: migrate stdlib and examples to value-defined generic functions` (plus fixtures and `Staple.md`)
5. *(optional)* `style: define monomorphic functions by value`

Commit 3 must include the test-source migrations it breaks, so every commit
passes `cargo test`.
