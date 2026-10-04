# Stage 1 Breakdown: `Forall` in the Syntax Tree

Part of [HYBRID_POLYMORPHISM_PLAN.md](HYBRID_POLYMORPHISM_PLAN.md).

## Goal

Move a generic function's compile-time parameter list out of `Binding` and
into its annotation, as a new `Type::Forall` node. The stage is
**behavior-preserving**:

- every program that compiles today compiles to the same LLVM IR;
- every program rejected today is still rejected, with the same diagnostic
  text wherever a test pins it;
- the surface syntax for `def` is unchanged, so `Staple.md` is not edited.

The only intended differences are listed under
[Intended differences](#intended-differences).

## Current Shape

- `Binding` (`staple-syntax/src/ast/item.rs`) has `type_parameters`,
  `trait_bounds`, and `subtype_bounds` beside `annotation: Option<Type>`.
- `Parser::parse_binding` and the extern-block member parser call
  `parse_bracketed_generics()` right after `:` and then `parse_type()`.
  `parse_binding` rejects parameters on non-`def` bindings ("compile-time
  parameters are only allowed on `def` bindings").
- `Parser::parse_companion` re-parses the companion's `<…>` clause once per
  member, for fresh `SyntaxId`s, and **prepends** those parameters and bounds
  to each member's `type_parameters`, `trait_bounds`, and `subtype_bounds`.
  It also stores a fresh `companion_target`.
- Macro declarations parse their own `<…>` before `parse_type()` and keep
  declaration-level fields. Trait members call `parse_type()` directly, so
  `<` there is a parse error today.
- `NameResolver::allocate_symbol` and `resolve_binding` (`resolve.rs`) copy
  the three fields into the `binding_type_parameters`,
  `binding_trait_bounds`, and `binding_subtype_bounds` side tables keyed by
  the binding's `SyntaxId`. `resolve_binding` declares the parameters in a
  fresh type-parameter scope before resolving the annotation and bounds.
  `ResolvedFunction` keeps its own copies of the three fields.
- Direct readers of the three `Binding` fields, outside the parser:
  - `typecheck.rs`: the generic-extern check (non-intrinsic ABIs) and three
    checks in `check_binding` ("generic value … requires a concrete expected
    type", "compile-time parameters require a function-valued `def`", "a
    generic `def` requires a function body");
  - `typecheck/parameter_products.rs`: capability analysis of generic
    bindings;
  - `lower.rs`: three `is_empty()` tests;
  - `macro_expand.rs`: `substitute_binding` and `freshen_binding`;
  - `expansion_render.rs`: renders `name<…>` after the binding name, which
    the parser no longer accepts ("compile-time parameters belong to the
    annotation");
  - LSP: `hover.rs` (signature rendering and parameter lookup),
    `definition.rs`, `semantic.rs`, `completion.rs`.
- Exhaustive matches over `Type` that need a new arm: two in `parser.rs`,
  nine in `macro_expand.rs` (including `substitute_type`, `freshen_type`,
  and the `Type` → `CompileType` conversion), two in `resolve.rs`
  (including `resolve_type`), one in `typecheck.rs`
  (`resolve_source_type_inner`), and one each in the LSP's `definition.rs`,
  `hover.rs`, and `semantic.rs`.
- `std.syntax` exposes `Type` as `opaque`, so macro reflection needs no new
  surface. The formatter works on the token stream and needs no structural
  change.
- Planned names use arena positions, not `SyntaxId`s, so allocating new
  syntax nodes does not change emitted symbol names.

## Target Shape

In `staple-syntax/src/ast/ty.rs`, next to `TraitBound` and `SubtypeBound`:

```rust
/// A `<…>` compile-time parameter list with its `where` clause.
pub struct GenericParameters {
    pub syntax: Syntax,
    pub parameters: Vec<TypeParameterPattern>,
    pub trait_bounds: Vec<TraitBound>,
    pub subtype_bounds: Vec<SubtypeBound>,
}

/// A generic function type, `<T where …> body`.
pub struct ForallType {
    pub syntax: Syntax,
    pub generics: GenericParameters,
    pub body: Box<Type>,
}

pub enum Type {
    // …existing variants…
    Forall(ForallType),
}
```

`Binding` loses `type_parameters`, `trait_bounds`, and `subtype_bounds` and
gains:

```rust
/// The enclosing companion's `<…>` clause, re-parsed for this member so its
/// `SyntaxId`s are fresh. Present only for companion members.
pub companion_generics: Option<GenericParameters>,
```

`companion_target` stays as it is. Accessors on `Binding` replace direct
field reads:

| Accessor | Meaning |
|---|---|
| `generics()` | The member's own parameters: the top-level `Forall` of `annotation`, if any |
| `annotation_body()` | `annotation` with a top-level `Forall` peeled off |
| `all_type_parameters()` | Companion parameters, then the member's own (today's merged order) |
| `all_trait_bounds()` / `all_subtype_bounds()` | The same, for bounds |
| `is_generic()` | `all_type_parameters()` is non-empty |

**`Display`.** `Type::Forall` renders as `<T, U where Bound T> body`, using
the same parameter and constraint rendering that `expansion_render` uses
today. A `Forall` in a function-parameter position is parenthesized, as a
nested function type is.

**Other declarations are unchanged.** Type, trait, `impl`, companion,
submodule, and macro declarations keep their declaration-level fields.
`GenericParameters` may replace those later, but not in this stage.

## Steps

Each step builds and passes `cargo test --workspace --quiet` before the next
starts.

### Step 1 — AST

- Add `GenericParameters`, `ForallType`, and `Type::Forall`, with
  `Type::syntax()` and `Display` arms.
- Add `companion_generics` and the accessors to `Binding`. Keep the old
  three fields for now, so the tree still builds, and have the accessors read
  them.
- Add the `Type::Forall` arm to every exhaustive match listed under Current
  Shape. Until Step 3, the arms are unreachable, because the parser never
  produces a `Forall`; they still have to be written correctly, since Step 3
  depends on them:
  - the `macro_expand` walkers (`substitute_type`, `freshen_type`) recurse
    into the parameters, bounds, and body;
  - the `Type` → `CompileType` conversion rejects a `Forall` with a
    diagnostic, since macro compile-time types are never generic functions;
  - LSP walkers recurse into the parameters, bounds, and body.

### Step 2 — Parser

- `parse_type()` accepts a `Forall` prefix: when the next token is `<`, it
  calls `parse_bracketed_generics()` and then parses the body with the
  existing function-type parser. The scope extends as far right as the body
  does, so a `Forall` in a parameter or product element needs parentheses,
  as in `(<T> move T -> T) -> I32`.
- `parse_binding` and the extern-member parser stop calling
  `parse_bracketed_generics()` themselves; the annotation's `parse_type()`
  produces the `Forall`. `SyntaxId`s are still allocated in source order.
  The new `GenericParameters` and `ForallType` nodes add ids, which is
  harmless because ids are only keys (see Risks).
- `parse_binding` keeps the "compile-time parameters are only allowed on
  `def` bindings" error, raised when a non-`def` binding's annotation is a
  top-level `Forall`.
- `parse_companion` stores each member's re-parsed clause in
  `companion_generics` instead of prepending it to the member's fields.
- Macro declarations keep calling `parse_bracketed_generics()` before
  `parse_type()`, so their annotations are unaffected.
- Stop filling the old three fields. The accessors now read
  `companion_generics` and the annotation.
- **Parser tests (`staple-syntax/tests/parser.rs`):**
  - `def` and extern annotations produce `Forall` with the right parameters
    and bounds, including `effect` parameters and `?Sized`;
  - companion members get `companion_generics` and keep their own `Forall`;
  - a parenthesized nested `Forall` parses in parameter, result, product
    element, and type-argument positions (later rejected by resolve);
  - `(a: <T> T -> T, b: I32)` ends the `Forall` at the comma;
  - the speculative type parse in `parse_expression` still handles
    `Type :: value`, and `f.<I32>` is unaffected;
  - a `Forall` explicit argument must be parenthesized, `f.<(<T> T -> T)>`
    (rejected by resolve until Stage 3), because `.<<` lexes as the `<<`
    operator;
  - `as Type` with a `<…>` target parses (later rejected by resolve);
  - the `let`/`const` error message is unchanged.

### Step 3 — Resolve

- `allocate_symbol` and `resolve_binding` fill the three side tables from
  `all_type_parameters()`, `all_trait_bounds()`, and `all_subtype_bounds()`,
  so every downstream reader sees today's merged lists unchanged.
- `resolve_binding` declares the companion parameters and then the
  annotation's own parameters in one scope, as today, and then resolves the
  annotation body and all bounds. The top-level `Forall`'s parameters must
  not be declared twice.
- `resolve_type` gets a `Type::Forall` arm for nested occurrences: push a
  scope, declare the parameters, resolve the bounds and body, and pop the
  scope. This keeps names resolvable so that the placement check below is
  the only diagnostic.
- `ResolvedFunction::binding_annotation` stores `annotation_body()`, and
  `ResolvedFunction`'s own three fields are filled from the merged
  accessors. The checker therefore sees exactly today's inputs.
- **Placement check.** A new pass reports a `Forall` anywhere except at the
  top of a `def` or extern binding's annotation, with "generic function
  types are not supported here yet". This covers nested positions, `let` and
  `const` (already caught by the parser), trait members, type declarations,
  ascriptions, and `as` targets. Trait members get a specific message:
  "trait members cannot introduce their own compile-time parameters yet".
  Stage 3 relaxes this pass, and Stage 7 relaxes it for trait members.

### Step 4 — Typecheck and lowering

- `resolve_source_type_inner` gets a `Type::Forall` arm that checks the body
  and returns its checked type, matching today's representation of a generic
  function type (free parameters). Only the top-level positions allowed by
  the placement check can reach it, and Stage 2 replaces the arm with
  `CheckedType::Forall`.
- Replace direct field reads with accessors:
  - the generic-extern check uses `binding.is_generic()`;
  - the three `check_binding` diagnostics use `is_generic()` and
    `annotation_body()`, keeping their exact text;
  - `parameter_products.rs` iterates the merged accessors;
  - lowering's three `is_empty()` tests use `is_generic()`.
- Any typecheck code that inspects `binding.annotation` as a function type
  uses `annotation_body()`.

### Step 5 — Macros and expansion rendering

- `substitute_binding` and `freshen_binding` drop their loops over the old
  fields. The annotation walk now covers the binding's own parameters and
  bounds through the `Forall` arms from Step 1. `companion_generics` is
  substituted and freshened explicitly, like `companion_target`.
- `expansion_render` stops emitting `name<…>` and renders the annotation
  through `Display`, so the output is `name: <T> …`, which the parser
  accepts. This fixes the stale rendering noted under Current Shape.
- Remove the old three fields from `Binding`. Every remaining reader now
  goes through an accessor.

### Step 6 — LSP and editor

- `hover.rs`: signatures come from `Display` of the annotation (which now
  includes `<…>`), so remove the separate parameter rendering and avoid
  printing the list twice. Parameter lookup for hover uses
  `all_type_parameters()`.
- `definition.rs`: go-to-definition on a type parameter finds its
  declaration inside the `Forall` or in `companion_generics`.
- `semantic.rs` and `completion.rs`: walk the `Forall` parameters, and
  confirm that synthesized companion-generic tokens are still ignored (see
  commit 50749ce).
- `editors/vscode/syntaxes/staple.tmLanguage.json`: no change expected,
  because `<T>` already appears in the same position. Confirm that
  `(<T> T -> T)` highlights sensibly; adjust only if it does not.

### Step 7 — Exit gate

- `cargo test --workspace --quiet` passes.
- Build a reference `staple` binary from the stage's base commit and run
  `scripts/compare-llvm-ir.py --old <reference> --new target/debug/staple
  --stdlib stdlib --runs 4` over the example and corpus programs. Every
  program reports `same`.
- Run the formatter over the stdlib and examples and confirm that the output
  is unchanged. In particular, check spacing for `(<T> …)`, which the
  token-based formatter has not seen before.
- Run the LSP tests, and smoke-test hover and go-to-definition on a generic
  `def`, a companion member, and a generic intrinsic.

## Intended Differences

- A `<…>` in a position other than the top of a `def` or extern annotation
  used to be a parse error. It now parses and is rejected by resolve, with
  "generic function types are not supported here yet" (or the trait-member
  message). Tests that pin the old parse error are updated to the new
  diagnostic.
- `expansion_render` output for generic bindings changes from `name<T>: …`
  to `name: <T> …`.

## Risks

- **`SyntaxId` shifts.** The new nodes allocate ids, which shifts every
  later id in a file. Ids are only keys, and planned names don't use them,
  but the LSP relies on the open file being parsed in the same order as the
  program (see the `parse_at` fix). Both paths use the same parser, so they
  shift together. Any test that pins raw id numbers needs updating, and
  Step 2 checks for such tests.
- **Double declaration.** Resolve must not declare the top-level `Forall`'s
  parameters both in `resolve_binding` and in the nested `resolve_type`
  arm. `resolve_binding` resolves `annotation_body()`, not the whole
  annotation.
- **Companion parameter order.** `all_type_parameters()` must yield the
  companion parameters first, then the member's own. Explicit `.<…>`
  arguments and `call_type_parameters` depend on that order.
- **Other `parse_type()` callers.** `parse_expression` tries `parse_type()`
  speculatively at every expression start, and explicit `.<…>` arguments
  call it once per argument. No expression or explicit argument begins with
  `<` today (prefix operators are only `-` and `!`), so accepting a `<`
  prefix can't change how existing code parses. The parser tests in Step 2
  pin both callers.
