# Stage 2 Breakdown: Lower Existing Typed Programs Completely

## Status and Goal

**Status:** In progress. Stage 1 is complete in commit `83b4872`; Stage 2.1, all of Stage 2.2, and Stage 2.3 are complete. Stage 2.4 (lower ordinary expressions and control flow) is next.

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

## Stage 2.1 - Establish the Arena and Core Schema (Done)

Define the foundational representation before lowering individual constructs.

- Add typed IDs and arenas for expressions, patterns, blocks, runtime items, function templates, and module initializers.
- Add `Origin`, `LoweredExpression`, `LoweredPattern`, `LoweredBlock`, `LoweredItem`, `LoweredFunction`, `LoweredInitializer`, and `LoweredProgram`.
- Give `LoweredExpression` its checked type, checked effects, optional coercion, moved-symbol set, and expression kind.
- Add stable references for `FunctionId`, `SymbolId`, `ModuleId`, `TypeId`, and `TraitMethodId`; do not replace existing semantic identities with arena positions.
- Keep all fields private or `pub(crate)` so the schema can evolve before the later feature is complete.
- Extend `LoweredModule` with the new program representation while retaining the private `TypedModule` bridge.

**Gate:** An empty or declaration-only checked program lowers into valid deterministic arenas, and the validator can walk every arena and reference.

Completed:

- Added distinct typed IDs and deterministic append-only arenas for expressions, patterns, blocks, runtime items, function templates, and module initializers.
- Added `Origin`, the foundational lowered node types, and `LoweredProgram`; expression nodes reserve explicit checked type, effect, coercion, and moved-symbol facts.
- Extended `LoweredModule` to own `LoweredProgram` while preserving the private typed-module backend bridge.
- Added a structural arena validator for every currently defined child relationship.
- Added unit coverage for empty arena validity, dense deterministic ID allocation, and dangling-reference diagnostics.
- Verified with `cargo fmt --all -- --check`, focused lowering tests, `cargo check --workspace`, `cargo test --workspace` (940 tests), and `git diff --check`.

## Stage 2.2 - Snapshot Modules, Functions, Symbols, and Type Metadata (Done)

The focused implementation sequence is maintained in [STAGE_2_2_METADATA_CATALOG_PLAN.md](STAGE_2_2_METADATA_CATALOG_PLAN.md).

- Lower source modules in initialization order while retaining their semantic `ModuleId` and qualified identity.
- Create one function template entry for every resolved function and implicit thunk, including checked signature, bounds, captures, parameter style, parameter symbols, source body reference, and compiler/intrinsic classification.
- Record symbol storage classification: immutable SSA value, mutable cell, global storage, function binding, derived binding, signal, captured cell, or external symbol.
- Copy the type/trait metadata needed by later lowering and specialization into owned compiler-facing tables rather than exposing `TypedModule` accessors through the IR.
- Record module initializers and their ordered runtime items separately from declaration-only source items.
- Preserve coroutine, reactive, resource, and standard-library semantic IDs needed by later substages.

**Gate:** Every runtime function, implicit thunk, symbol, and module initializer has exactly one lowered catalog entry, with duplicate/missing-ID validation.

Progress:

- Step 1 added typed catalog IDs and insertion-ordered catalogs for modules, functions, symbols, types, traits, and trait methods, plus an ordered trait-implementation arena.
- Semantic IDs are indexed separately from arena positions; duplicate insertion returns a source-based diagnostic without overwriting the first entry.
- Validation now checks every semantic lookup against ordered entries in both directions and diagnoses stale, mismatched, or dangling indexes.
- Focused tests cover insertion order, duplicate rejection, retained original values, and bidirectional lookup disagreement. `cargo fmt --all -- --check`, `cargo check --workspace`, focused lowering tests, and `git diff --check` pass.
- Step 2 added sorted resolver inventories for symbols, type parameters, types, traits, and trait methods without exposing their backing maps.
- Added deterministic checked inventories for implicit thunks, derived evaluators, method signatures, trait parameter templates, functional dependencies, and implementations.
- Type checking now retains all selected standard/runtime semantic IDs required by later lowering. Compact checked type-representation templates remain assigned to Step 6 after transition testing showed that retaining fully expanded representation trees is unsafe for recursive standard-library metadata.
- Focused coverage verifies completeness and strict semantic-ID ordering against a checked standard-library program.
- Boxed the private legacy `TypedModule` bridge to keep the growing lowered owner off constrained test-thread stack frames; the isolated block-scoped module regression and full workspace suite pass at the default stack size.
- Verified Step 2 with `cargo fmt --all -- --check`, `cargo check --workspace`, focused lowering tests, `cargo test --workspace` (943 tests), and `git diff --check`.
- Step 3 traversed `Program::initialization_order()` to insert one `LoweredModuleInfo` per module in initialization order, retaining semantic ID, qualified name, parent, companion status, initialization index, executable-entry flag, and initializer ID.
- Added order validation that reports unknown, duplicate, and missing module IDs; module origins prefer the declaration syntax and fall back to the module syntax origin for file modules without one.
- Every module now receives one empty `LoweredBlock` and one `LoweredInitializer`. Initializers record ordered `RuntimeItemSource` entries for runtime top-level items and entry IO/reactive `CheckedResource` metadata; declaration-only items remain outside the initializer.
- Added focused coverage for multi-module initialization order, declaration-only modules, source-order runtime categories, companion parentage, file-module origins, entry IO metadata, repeated-lowering stability, and order diagnostics.
- Verified Step 3 with `cargo fmt --all -- --check`, `cargo check --workspace`, focused lowering tests, `cargo test --workspace` (951 tests), and `git diff --check`.
- Step 4 populated one lowered function template for every declared function and implicit thunk: declared functions in resolver order, then thunks in ascending `FunctionId`.
- Templates copy the checked function type, checked trait bounds, parameter style, recursively collected parameter symbols in source order, body origin/syntax, and owning module without cloning the body expression.
- Captures retain independent borrowed, non-owning, and shared-cell requirements. Thunks carry orthogonal declared/implicit/derived/coroutine/effectful-helper/extern/intrinsic classification flags.
- Coroutine body blocks now record their module during resolution so thunk bodies resolve their owning module.
- Added focused coverage for catalog order and uniqueness, checked signature/bounds/parameters, capture ownership facts, derived and coroutine classification, and effectful callback classification.
- Verified Step 4 with `cargo fmt --all -- --check`, `cargo check --workspace`, focused lowering tests, `cargo test --workspace` (956 tests), and `git diff --check`.
- Step 5 added `LoweredSymbol` and `SymbolStorage` with the documented storage precedence: external, function/constructor/singleton binding, derived binding, signal, captured mutable cell, global storage, mutable cell, immutable value.
- The resolver symbol inventory now carries declaration syntax, source span, module, owner, and a module-symbol flag; constructors and singletons record declarations too.
- Compile-time-only symbols (consts and compiler-owned syntax constructors) stay out of the catalog; every other declared runtime symbol is inserted in ascending `SymbolId`, with referenced parameters/captures supplemented.
- Mutation, moves, initialization checking, derivation, signal behavior, and captured-cell use remain independent flags, and optional function/constructor/singleton/intrinsic/external identities are recorded.
- Validation covers function parameter, capture, and binding symbol references plus top-level runtime initializer binding symbols.
- Added focused coverage for globals, locals, mutable parameters, captured cells, borrowed captures, functions, externs, intrinsics, signals, derived bindings, constructors, singletons, and macro quote exclusion.
- Verified Step 5 with `cargo fmt --all -- --check`, `cargo check --workspace`, focused lowering tests, `cargo test --workspace` (962 tests), and `git diff --check`.
- Step 6 populated the type catalog with declaration kind, builtin/recursive classification, checked parameter templates, and compact representation templates where nested nominal types stay references rather than expanded representations.
- Added trait, trait-method, and trait-implementation metadata: names, modules, parameter templates, prerequisites, functional dependencies, declared method order, default functions, implementation arguments/bounds, negative flags, and selected method functions.
- Type checking records representation templates, type parameter templates, and trait prerequisites after its diagnostics gate; resolver trait implementations now carry declaration spans for origins.
- A first placement of template collection before checking perturbed the `resolved_named_types` memo and suppressed cyclic-type diagnostics; the final placement preserves acceptance behavior exactly.
- Added focused coverage for type ordering/template compactness, trait parameters/dependencies/methods/defaults, and implementation data including negation.
- Verified Step 6 with `cargo fmt --all -- --check`, `cargo check --workspace`, focused lowering tests, `cargo test --workspace` (965 tests), and `git diff --check`.
- Step 7 populated `LoweredSemanticIds` from type-checker-owned selections: standard traits, runtime subsystem types, canonical IO/reactive resources, string representation, and the entry-reactive requirement.
- Absent subsystems remain `None` for `no_prelude` or library-only programs; no lowering code looks any name up again.
- Validation now rejects semantic IDs without matching catalog records and checks IO/reactive resources against their selected type IDs.
- Added focused coverage for ordinary and coroutine-imported programs, `no_prelude`, the reactive entry requirement, and invalid semantic IDs.
- Verified Step 7 with `cargo fmt --all -- --check`, `cargo check --workspace`, focused lowering tests, `cargo test --workspace` (969 tests), and `git diff --check`.
- Step 8 expanded validation across every catalog: duplicate semantic IDs, lookup agreement, initialization indices and parents, exactly one initializer per module, unique runtime-item sources, function module/body/parameter/capture/binding references, symbol module/owner/target references with storage consistency, type module references, trait/method/implementation cross-references, and semantic-ID catalog families.
- Added normalized repeated-lowering snapshots over all catalogs and a transition test comparing every semantic-ID field, resource, string representation, and entry-reactive flag with `TypedModule`.
- Added corruption tests for invalid module parents, duplicate initializers, and symbol module references.
- Verified Step 8 with `cargo fmt --all -- --check`, `cargo check --workspace`, focused lowering tests, `cargo test --workspace` (972 tests), and `git diff --check`.
- **Stage 2.2 is complete. Stage 2.3 is next: lower patterns, places, and runtime items into the arenas.**

> **Complexity note:** The metadata currently lives across resolver, type-checker, ownership, reactive, and coroutine side tables. This substage may need a focused inventory plan before implementation begins.

## Stage 2.3 - Lower Patterns, Places, and Runtime Items (Done)

- Lower wildcard, binding, product, nominal, literal, reference/slice, and at-pattern forms with checked types and bound symbols.
- Normalize assignment targets into explicit place operations: symbol storage, dereference, product element, representation access, slice/index access, and captured cell.
- Lower runtime bindings, pattern bindings, assignments, returns, breaks, continues, and expression statements.
- Attach initialization checks, mutation/writeback behavior, propagation metadata, and pattern match metadata at their use sites.
- Reject any macro, import, declaration, visibility splice, unresolved unary/binary node, or other compile-time-only item that unexpectedly survives into a runtime block.

**Gate:** All runtime items and patterns can be reconstructed from lowered arenas without consulting their source AST nodes.

Completed:

- Added a `PlaceId` arena with `LoweredPlace`/`LoweredPlaceKind` and expanded `LoweredPatternKind` to wildcard, binding, product, nominal, literal, and at forms. Every pattern carries its checked type, bound symbols, singleton targets, and typed child pattern IDs.
- Assignment targets normalize to explicit place trees: symbol storage, captured cell, temporary (a non-place indexed base), resource, dereference across `Ref` payloads, product element (including bounds-checked `Slice` elements), representation, and `MutateIndex`-dispatched indexed access.
- Replaced the Stage 2.2 `RuntimeItemSource` roots with lowered items. Module initializers now own their ordered runtime items in their body block, and every function template records its lowered parameter pattern and body block.
- Extended `LoweredItemKind` with binding, pattern-binding, assignment, return, break, continue, and expression-statement payloads. Binding items record compile-time-only/generic/derived/signal/cell/initialization facts; pattern bindings record checked propagation metadata; assignment items record the selected `MutateIndex` dispatch, initialization-state symbol, previous-value drop, and signal writeback; expression statements record discard-drop behavior.
- Stage 2.3 allocates expression headers (origin, checked type, effects, coercion, moved symbols) for runtime-item payloads and place bases and lowers block expressions into item sequences plus a tail result. `LoweredExpressionKind::Unlowered` marks the families Stage 2.4 replaces; the expression arena is memoized by syntax ID so later stages reuse the same node.
- Compile-time-only source items (declarations, imports, modifiers, and expanded item-producing macro markers) are omitted exactly as resolution and code generation treat them. Unexpanded visibility/repeated item splices, unresolved unary/binary operator expressions, and unexpanded quote/splice/syntax-argument expressions are lowering diagnostics instead of backend panics.
- Type checking now records the resolved `MutateIndex` output type on indexed assignment targets and the checked type of a propagating binding's nominal root; compiler-synthesized implicit-thunk parameter patterns fall back to the checked signature parameter and the thunk body origin.
- Expressions whose checking diverged record no type; lowering treats them as unreachable `Never` values, matching the checker's divergence handling.
- Validation now checks every new pattern, place, item, and function-parameter-pattern reference. Repeated-lowering snapshots include expressions, patterns, places, blocks, and items.
- Main-branch ownership updates are reflected in the snapshot: nominal destructuring patterns preserve their whole-pattern `move` marker, while consuming constructor and iterator contracts flow through the checked function signatures already stored by Stage 2.2/2.3.
- Break/continue item kinds are implemented but populate once loop-body blocks lower in Stage 2.4, since loop bodies are expressions; match-arm patterns and `CheckedMatch` metadata likewise arrive with match-expression lowering in Stage 2.4.
- Added focused coverage for every parameter pattern form, pattern-binding items (including at and propagation), all place operations, captured-cell and resource places, module binding/assignment/statement metadata, function body/result normalization, compile-time-only rejection, dangling-reference validation, and deterministic repeated lowering.
- Verified after the `main` merge with `cargo fmt --all -- --check`, focused nominal-pattern and slice-iteration tests, `cargo test --workspace` (989 tests), and `git diff --check`.

## Stage 2.4 - Lower Ordinary Expressions and Control Flow

The focused implementation sequence is maintained in [STAGE_2_4_EXPRESSION_CONTROL_FLOW_PLAN.md](STAGE_2_4_EXPRESSION_CONTROL_FLOW_PLAN.md).

Progress:

- Step 1 added a single `classify_expression` dispatcher that assigns every `Expression` variant exactly one decision: a Stage 2.4 family, an explicit Stage 2.5/2.6 deferral (`Callable`, `Resource`, `Coroutine`), or a compile-time-only rejection. `Unlowered` is gone; families whose concrete payload has not landed yet are explicit per-family `Pending` markers.
- Expression memoization is now occurrence-aware: `ExpressionKey { syntax, owner, context }` distinguishes module initializers from function templates and gives contextual/default occurrences distinct keys through the consuming product and destination slot. Block allocations are memoized with the same key so loop bodies and function bodies reuse one arena node.
- Ordinary children are lowered recursively before their parent is allocated, so a diagnostic never leaves a partially initialized node. Loop bodies, match subjects and arm bodies, product elements, spreads, access bases, index operands, logical operands, and template interpolations are all visited.
- Added a coverage classifier test with one constructed representative per syntax variant; the exhaustive classifier match and variant-name match fail compilation when a new variant appears without a decision.
- Verified with `cargo fmt --all -- --check`, `cargo check --workspace`, `cargo test -p staple-compiler lower::tests` (48 tests), `cargo test --workspace`, and `git diff --check`.

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
