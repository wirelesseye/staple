# Stage 2 Breakdown: Lower Existing Typed Programs Completely

## Status and Goal

**Status:** Not started. Stage 1 is complete in commit `83b4872`.

Stage 2 will construct a complete, owned, typed IR for every runtime-relevant part of a successfully checked program. The existing LLVM backend will continue using the private legacy `TypedModule` bridge during this stage; Stage 5 will migrate the backend and remove that bridge.

Stage 2 is complete when every accepted program produces validated lowered arenas containing all semantic decisions that can be recorded before specialization. It must not change source-language behavior, reachability, specialization, or ABI.

## Required IR Invariants

- `LoweredModule` owns a `LoweredProgram` plus the temporary private legacy backend payload. Only `lower.rs` may construct either representation.
- Every lowered node carries an `Origin` containing its `SyntaxId` and `Span`. Compiler-synthesized nodes use `Span::Compiler` and retain the nearest meaningful syntax ID when one exists.
- Child relationships use typed arena IDs rather than cloned syntax expressions, patterns, or items.
- Runtime types are `CheckedType` values. No source `Type`, inference request, unresolved overload set, or name lookup reaches the lowered arenas.
- Generic function bodies remain templates in Stage 2 and may contain declared type/effect parameters. Stage 3 is responsible for producing concrete instances.
- Every call records its resolved call category, checked function type, inferred substitutions available at that site, ordered effect resources, mutation/move behavior, and trait-evidence recipe where applicable.
- Ownership facts are recorded explicitly on lowered nodes and function metadata. LLVM-specific control flow, storage layout, and cleanup instructions remain outside this IR.
- Source-only declarations such as imports, macro definitions, and type syntax are omitted unless runtime metadata or initialization ordering needs a normalized representation.
- Arena iteration and all declaration catalogs preserve deterministic program/source order.

## Stage 2.1 - Establish the Arena and Core Schema

Define the foundational representation before lowering individual constructs.

- Add typed IDs and arenas for expressions, patterns, blocks, runtime items, function templates, and module initializers.
- Add `Origin`, `LoweredExpression`, `LoweredPattern`, `LoweredBlock`, `LoweredItem`, `LoweredFunction`, `LoweredInitializer`, and `LoweredProgram`.
- Give `LoweredExpression` its checked type, checked effects, optional coercion, moved-symbol set, and expression kind.
- Add stable references for `FunctionId`, `SymbolId`, `ModuleId`, `TypeId`, and `TraitMethodId`; do not replace existing semantic identities with arena positions.
- Keep all fields private or `pub(crate)` so the schema can evolve before the later feature is complete.
- Extend `LoweredModule` with the new program representation while retaining the private `TypedModule` bridge.

**Gate:** An empty or declaration-only checked program lowers into valid deterministic arenas, and the validator can walk every arena and reference.

## Stage 2.2 - Snapshot Modules, Functions, Symbols, and Type Metadata

- Lower source modules in initialization order while retaining their semantic `ModuleId` and qualified identity.
- Create one function template entry for every resolved function and implicit thunk, including checked signature, bounds, captures, parameter style, parameter symbols, source body reference, and compiler/intrinsic classification.
- Record symbol storage classification: immutable SSA value, mutable cell, global storage, function binding, derived binding, signal, captured cell, or external symbol.
- Copy the type/trait metadata needed by later lowering and specialization into owned compiler-facing tables rather than exposing `TypedModule` accessors through the IR.
- Record module initializers and their ordered runtime items separately from declaration-only source items.
- Preserve coroutine, reactive, resource, and standard-library semantic IDs needed by later substages.

**Gate:** Every runtime function, implicit thunk, symbol, and module initializer has exactly one lowered catalog entry, with duplicate/missing-ID validation.

> **Complexity note:** The metadata currently lives across resolver, type-checker, ownership, reactive, and coroutine side tables. This substage may need a focused inventory plan before implementation begins.

## Stage 2.3 - Lower Patterns, Places, and Runtime Items

- Lower wildcard, binding, product, nominal, literal, reference/slice, and at-pattern forms with checked types and bound symbols.
- Normalize assignment targets into explicit place operations: symbol storage, dereference, product element, representation access, slice/index access, and captured cell.
- Lower runtime bindings, pattern bindings, assignments, returns, breaks, continues, and expression statements.
- Attach initialization checks, mutation/writeback behavior, propagation metadata, and pattern match metadata at their use sites.
- Reject any macro, import, declaration, visibility splice, unresolved unary/binary node, or other compile-time-only item that unexpectedly survives into a runtime block.

**Gate:** All runtime items and patterns can be reconstructed from lowered arenas without consulting their source AST nodes.

## Stage 2.4 - Lower Ordinary Expressions and Control Flow

Lower expression families in dependency order:

1. Literals, names, resources, and representation/product access.
2. Products, repeated products, spreads, blocks, and `satisfies` coercions.
3. Logical expressions, loops, matches, and propagation paths.
4. Indexing and mutation-related expression forms.
5. Strings, C strings, and string-template parts.

For each expression, store the already selected access/coercion/match/logical plan directly on the lowered node. Preserve evaluation order exactly, including designated fields, spreads, defaults, short-circuiting, and early exits.

**Gate:** Every non-call, non-closure expression accepted by type checking has a lowered form, and a coverage test fails when a new syntax variant lacks an explicit lowering decision.

> **Complexity note:** Products, defaults, sums, propagation, and control-flow ownership interact heavily. These families may need individual implementation plans if the lowering functions become too broad.

## Stage 2.5 - Lower Calls, Trait Evidence, and Closures

- Introduce explicit callable categories for known functions, indirect closures, externs, intrinsics, constructors, trait implementations, structural trait methods, and compiler helpers.
- Lower ordinary, curried-default, juxtaposed, companion-method, constructor, and intrinsic calls without reconstructing their checked plans later.
- Record the checked function type at each call after call-site inference, along with ordered arguments, hidden resources, mutations, moves, and initialization checks.
- Represent trait selection as an evidence recipe:
  - selected explicit implementation and method function;
  - selected structural implementation;
  - declared generic bound/prerequisite to be realized after substitution.
- Lower function expressions and function-valued names into closure-construction nodes containing function identity, captures, capture ownership/borrowing, environment-sharing requirements, and adapter kind.
- Record compile-time substitutions known at the use site without assigning `FunctionInstanceId`; Stage 3 owns instance interning and worklist discovery.

**Gate:** Every call and function value has exactly one explicit callable category and enough checked information for Stage 3 to resolve an instance without inspecting expression types or resolver maps.

> **Complexity note:** This is Stage 2's highest-risk substage. Curried calls, generic captures, trait functional dependencies, defaults, and structural evidence may require separate breakdown plans.

## Stage 2.6 - Lower Resources, Reactive Operations, and Coroutines

- Lower `with` and resource access with canonical resource identity, mutability, lexical scope, and ordered effect-row requirements.
- Record signal, derived binding, reaction, batching, and initialization metadata currently recovered by codegen.
- Move coroutine plans into owned lowered records: body function, captures, deferred effects, result type, resume points, frame bindings, awaited result types, and cancellation classifications.
- Lower `coro` and `await` nodes to explicit coroutine operations while retaining the current statement-position and frame semantics.
- Record all implicit thunk relationships used by resources, reactive operations, derived bindings, and coroutines.

**Gate:** Lowered IR contains every semantic input used by the current reactive and coroutine backend paths, verified by targeted comparison tests against `TypedModule` during the transition.

> **Complexity note:** Reactive and coroutine lowering each span several backend subsystems. Either subsystem may need its own implementation plan before this substage starts.

## Stage 2.7 - Complete Validation, Coverage, and Transition Checks

- Expand validation to check arena bounds, unique semantic IDs, function/body ownership, child-node ownership, concrete non-template metadata, callable completeness, trait evidence, symbol availability, capture consistency, and initializer ordering.
- Add a traversal that proves every runtime source node belongs to exactly one lowered owner and has a lowered counterpart.
- During the legacy transition, add test-only comparisons for selected types, effects, call plans, ownership facts, captures, and coroutine plans so divergence is detected before Stage 5.
- Keep lowering deterministic and return diagnostics with source origins instead of panicking on malformed compiler state.
- Update the main plan's status and completed-verification sections after every merged substage.

**Gate:** The full workspace suite lowers every successful fixture, all new validator/coverage tests pass, and no accepted runtime construct is represented only in the legacy payload.

## Testing Matrix

- Arena and validation unit tests: empty modules, multiple source modules, implicit thunks, invalid IDs, dangling children, duplicate owners, and deterministic order.
- Expression coverage tests: one accepted fixture per lowered expression and runtime item variant.
- Call tests: direct, indirect, extern, intrinsic, generic, curried, juxtaposed, constructor, trait, structural trait, companion, and initialized global calls.
- Ownership tests: moves, mutations, borrowed and owned captures, captured cells, early returns, propagation, loop exits, and cleanup-relevant values.
- Runtime subsystem tests: resources, signals, reactions, derived bindings, coroutine suspension/resumption/completion/cancellation, and cross-module initialization.
- Regression commands after every substage:

  ```text
  cargo fmt --all -- --check
  cargo check --workspace
  cargo test --workspace
  git diff --check
  ```

## Explicit Non-Goals

- Do not move specialization discovery out of LLVM; that is Stage 3.
- Do not precompute the complete compiler-helper catalog; that is Stage 4.
- Do not migrate LLVM emission to lowered-node traversal or remove the private legacy payload; that is Stage 5.
- Do not add schemes, stored polymorphic values, erased calls, runtime dictionaries, type descriptors, or ABI adaptations for the later feature.

## Definition of Done

- `LoweredModule` owns a complete validated `LoweredProgram` for all existing runtime constructs.
- All semantic call, trait, closure, ownership, resource, reactive, and coroutine decisions available before specialization are explicit in that program.
- The only remaining use of the cloned `TypedModule` is the private Stage 5 backend bridge.
- The main plan identifies Stage 3 as next and records all completed Stage 2 verification.
