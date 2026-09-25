# Add a Typed Lowering and Specialization Phase

## Summary

Introduce a behavior-preserving compiler phase:

```text
resolve -> type check/ownership -> lower and specialize -> LLVM
```

LLVM generation will consume lowered IR and will no longer infer types, select trait implementations, or discover generic instances. This update does not add polymorphic values, erased dictionaries, runtime descriptors, or ABI changes.

## Current Status

- **Stage 1 is complete.** The public lowering boundary exists, compilation modes invoke it, and code generation accepts only `LoweredModule`.
- `LoweredModule` is currently an opaque owner of a cloned `TypedModule`. `CodeGenerator` crosses a private, explicitly transitional bridge to the existing backend implementation.
- The initial validator rejects functions or implicit thunks that have no checked function type.
- All CLI and compiler code-generation tests now pass through `Lowerer`.
- `cargo check --workspace` and `cargo test --workspace` pass. The workspace test run covers 937 tests.
- **Stage 2 is complete.** Stage 2.1 through Stage 2.7 are done. Lowering owns validated declaration catalogs plus lowered patterns, assignment places, runtime items, function bodies, module initializer bodies, concrete lowered forms for every ordinary expression and control-flow family (literals, names, access, products/spreads/defaults, blocks, `satisfies`, logicals, loops, matches, indexing, and string templates), explicit callable, call, closure-construction, substitution, and trait-evidence records for every call and function value, and owned resource providers/uses, `with` scopes, signal/derived/reactive operations, coroutine plans, `coro` creations, and ordered `await` sites. Every `Expression` variant has one exhaustive owned/rejected decision with no remaining deferral; validation covers arena identity and bounds, single-owner attribution, function/body and capture consistency, concrete runtime metadata, callable completeness, trait evidence, literal payloads, provider/use consistency, reactive identity, effect-resource ordering, and coroutine body/thunk/await consistency; a source-coverage traversal proves every runtime construct has exactly one lowered counterpart; and repeated lowering is deterministic. Stage 3 (structural instance keys and the specialization worklist) is next; Stage 5 still removes the temporary legacy payload after migrating the backend.
- The detailed Stage 2 implementation sequence is maintained in [STAGE_2_LOWERING_BREAKDOWN.md](STAGE_2_LOWERING_BREAKDOWN.md).

## Public Interfaces

- Add an owned, opaque `LoweredModule` and public entry point:

  ```rust
  Lowerer::new().lower(&TypedModule)
      -> Result<LoweredModule, Vec<Diagnostic>>
  ```

- Change `CodeGenerator::{compile_module, compile_module_for_target, emit_object}` to accept `&LoweredModule`.
- Do not retain a backend path from `TypedModule` directly to LLVM.
- CLI `check` mode stops after type checking; compilation modes invoke lowering before code generation.

## Implementation Stages

### Stage 1 - Define the Lowering Pipeline Boundary (Done)

- Added `lower.rs` with public `Lowerer` and opaque, owned `LoweredModule` types.
- Added the `Lowerer::new().lower(&TypedModule) -> Result<LoweredModule, Vec<Diagnostic>>` boundary and initial checked-function validation.
- Updated CLI compilation and all code-generation test entry points to lower successfully checked modules first. CLI `check` mode still stops after type checking.
- Changed all public `CodeGenerator` entry points to accept `&LoweredModule`; direct public emission from `TypedModule` is no longer available.
- Kept the old backend reachable only through a private transitional `LoweredModule::typed` bridge. Removing this bridge requires the explicit arenas and metadata introduced in Stage 2.

### Stage 2 - Lower Existing Typed Programs Completely (Done)

- Add owned, arena-backed lowered modules, expressions, patterns, initializers, callable targets, function templates, trait evidence, closure construction, ownership facts, and helper requirements alongside the temporary legacy backend payload.
- Preserve source spans and syntax IDs on lowered nodes for diagnostics.
- Lower every current expression, pattern, binding, initializer, implicit thunk, and coroutine plan.
- Record concrete type/effect information, coercions, accesses, selected symbols, storage requirements, ownership operations, and ordered resource arguments directly on lowered nodes.
- Represent calls explicitly as:
  - direct known-function calls, with instances assigned in Stage 3;
  - indirect concrete-closure calls;
  - external calls;
  - intrinsics;
  - concrete trait implementation calls;
  - structural trait calls.
- Represent closure creation with its function target, capture order, capture ownership, environment inputs, and required adapter kind; Stage 3 assigns concrete code instances.
- Carry enough cleanup and ownership metadata for the backend to preserve moves, borrows, drops, early returns, propagation, and cancellation without querying `TypedModule`.
- Expand the validator to cover dangling arena references, missing or unresolved targets, incomplete evidence, and invalid instance references.
- Follow the detailed sequence and gates in [STAGE_2_LOWERING_BREAKDOWN.md](STAGE_2_LOWERING_BREAKDOWN.md).

Progress:

- Stage 2.1 established typed IDs and append-only arenas for expressions, patterns, blocks, runtime items, function templates, and module initializers.
- Added `Origin` and the foundational lowered node/program schemas, with checked type/effect/coercion and moved-symbol slots on expressions.
- `LoweredModule` now owns the explicit `LoweredProgram` alongside the private Stage 5 backend bridge.
- Added deterministic-arena and dangling-reference validator tests. Stage 2.2 will populate declaration catalogs and metadata.
- Stage 2.2 Step 1 added ordered module, function, symbol, type, trait, trait-method, and trait-implementation catalog foundations. Semantic IDs remain distinct from arena IDs, duplicate insertions diagnose instead of overwrite, and lookup maps are validated against their ordered entries.
- Stage 2.2 Step 2 added narrow semantic-ID-ordered inventories for symbols, type parameters, types, traits, trait methods, implicit thunks, derived evaluators, checked trait metadata, and the selected standard/runtime semantic IDs. Compact non-expanding type representation templates remain part of Step 6; retaining fully expanded `CheckedType` representations was rejected because recursive standard-library metadata overflowed ordinary test-thread stacks when cloned or dropped.
- Stage 2.2 Step 3 populated the module catalog in initialization order and allocated one empty block plus one initializer per module. Initializers now own ordered `RuntimeItemSource` roots for runtime top-level items, and the executable entry records IO/reactive resource metadata. Unknown, duplicate, and missing initialization-order IDs are lowering diagnostics.
- Stage 2.2 Step 4 populated function templates for every declared function and implicit thunk with checked signatures, bounds, parameter symbols, captures and ownership flags, body origins, owning modules, and declared/thunk/derived/coroutine/resource-helper/extern/intrinsic classification.
- Stage 2.2 Step 5 populated the runtime symbol catalog in ascending `SymbolId` with declaration origins, modules, owners, checked types, primary storage classification, orthogonal mutation/move/initialization/capture flags, and optional function/constructor/singleton/intrinsic/external targets. Compile-time-only consts and syntax constructors stay out; function parameter, capture, and binding references are validated against the catalog.
- Stage 2.2 Step 6 populated type and trait catalogs. Types carry kind, builtin/recursive classification, checked parameter templates, and compact representation templates that reference nested nominal types by ID. Traits carry parameter templates, prerequisites, functional dependencies, declared method order, defaults, and checked implementations with arguments, bounds, negation, and selected method functions.
- Stage 2.2 Step 7 populated type-checker-selected standard/runtime semantic IDs, canonical IO/reactive resources, the string representation, and the entry-reactive requirement. Absent subsystems stay `None`; present IDs are validated against the trait/type catalogs without name-based rediscovery.
- Stage 2.2 Step 8 completed catalog validation (unique semantic IDs, lookup agreement, module parents and one-initializer-per-module, function/symbol/type/trait cross-references, semantic-ID families) and added normalized repeated-lowering snapshots plus `TypedModule` transition comparisons. Stage 2.2 is complete. Stage 2.3 begins lowering patterns, places, and runtime items into the existing arenas.
- The private legacy `TypedModule` payload is now boxed inside `LoweredModule`, reducing transitional stack-frame pressure without changing the public lowering boundary or backend behavior.
- Stage 2.3 expanded the lowered arenas: patterns now cover wildcard, binding, product, nominal, literal, and at forms with checked types and bound symbols; a new place arena normalizes assignment targets into symbol storage, captured cell, temporary, resource, dereference, product element, representation, and `MutateIndex`-dispatched indexed access.
- Runtime items replace the Stage 2.2 source roots: module initializers own their ordered items, every function template records its parameter pattern and lowered body block, and items carry binding, pattern-binding, assignment, return, break, continue, and expression-statement metadata (initialization, propagation, `MutateIndex` dispatch, previous-value drop, and signal writeback).
- Stage 2.3 allocates expression headers with checked type/effects/coercion/moved symbols for runtime-item payloads and place bases, lowering block expressions into item sequences plus a tail result; `Unlowered` kinds mark the families Stage 2.4 replaces. Compile-time-only source items are omitted, while unexpanded splices, operator expressions, and quote/splice nodes become lowering diagnostics.
- Type checking now records `MutateIndex` output types on indexed assignment targets and propagating nominal root pattern types; compiler-synthesized implicit-thunk parameters fall back to the checked signature parameter and body origin, and diverged expressions lower as unreachable `Never` values.
- Stage 2.3 validation checks every new pattern, place, item, and function-parameter-pattern reference, and repeated-lowering snapshots now include expressions, patterns, places, blocks, and items.
- After merging the consuming-iterator and nominal-destructure ownership changes from `main`, nominal patterns retain their whole-pattern `move` marker in lowered IR. Existing checked function signatures continue to carry the updated constructor and iterator move contracts, so later specialization and cleanup stages do not need to rediscover those ownership decisions.

> **Complex stage:** The AST and backend support many specialized constructs, including defaults, reactive bindings, structural indexing, ownership cleanup, and coroutines. This stage may need separate breakdown plans by expression family and runtime subsystem during implementation.

### Stage 3 - Add Structural Instance Keys and the Specialization Worklist (Remaining)

- Keep generic definitions as lowering-time templates and emit fully substituted `LoweredFunctionInstance` bodies.
- Define `InstanceKey` from:
  - function identity;
  - canonical substitutions for every relevant inner and outer type/effect parameter;
  - selected trait evidence when it affects generated code.
- Canonical keys must use semantic IDs and structural type data, excluding display names, contextual defaults, and debug formatting.
- Add typed keys for constructor adapters and structural trait implementations.
- Intern each key before lowering its body so same-specialization recursion terminates.
- Discover dependencies deterministically from calls, function values, closures, captures, defaults, trait methods, and compiler-generated operations.
- Preserve current roots: all currently emitted nongeneric functions and module initializers remain roots; generic bodies are emitted only for reachable concrete uses.
- Assign deterministic instance IDs and generated symbol names based on stable traversal and semantic keys.

> **Complex stage:** Nested generic closures, captured outer parameters, result-only parameters, effects, trait prerequisites, and recursive references can affect instance identity. This stage may require its own breakdown plan covering key construction, free-parameter collection, and worklist convergence.

### Stage 4 - Record Compiler-Generated Artifacts Before LLVM (Remaining)

- Add all implicit artifacts to the lowered catalog before emission:
  - constructor and concrete closure adapters;
  - structural trait implementations;
  - trait default and selected implementation methods;
  - drop, debug, indexing, and formatting dependencies;
  - reactive and derived thunks;
  - coroutine resume and cleanup functions;
  - layout-specific helpers required by lowered operations.
- Deduplicate artifacts using structural keys.
- Validate that every lowered callable or helper reference resolves to a catalog entry.

> **Complex stage:** Some dependencies are currently discovered deep inside LLVM emission, especially formatting, structural traits, cleanup, and coroutines. This stage may need subsystem-specific breakdown plans to identify and relocate every hidden discovery path.

### Stage 5 - Migrate LLVM Generation to Lowered IR (Remaining)

- Predeclare all functions, adapters, and helpers from the lowered catalog.
- Emit bodies in deterministic instance order.
- Replace AST traversal and `TypedModule` side-table queries with lowered-node traversal.
- Remove the private transitional `LoweredModule::typed` bridge once its final consumer has migrated.
- Remove:
  - the LLVM-time specialization queue;
  - active type substitutions;
  - expression type overrides;
  - generic type-argument reconstruction;
  - LLVM-time trait selection;
  - debug-string specialization keys.
- Keep target-specific LLVM type layout, calling-convention construction, and instruction emission in the backend.
- Do not merge until every backend path uses lowered IR; no mixed AST/IR fallback may remain.

> **Complex stage:** This is the largest mechanical migration and touches most of the backend. It may require breakdown plans organized around functions/closures, aggregates/control flow, ownership, traits/effects, reactive code, and coroutines.

### Stage 6 - Remove Transitional Code and Document the Boundary (Remaining)

- Remove obsolete `TypedModule` accessors used only by the old backend path while retaining APIs needed by diagnostics, tooling, and lowering.
- Add module-level documentation describing phase responsibilities and invariants.
- Confirm that lowering failures produce source-based diagnostics rather than backend panics.
- Verify that concrete closure, resource, coroutine, FFI, and ownership ABIs are unchanged.

## Test Plan

### Completed Verification

- `cargo check --workspace`
- `cargo test --workspace` (937 tests passing)
- Existing CLI compile/run, object emission, LLVM verification, module, ownership, trait, reactive, and coroutine coverage now exercises the lowering boundary.
- Stage 2.1 focused tests cover empty deterministic arenas, dense insertion-ordered typed IDs, and dangling child detection. `cargo fmt --all -- --check`, `cargo check --workspace`, `cargo test --workspace` (940 tests), and `git diff --check` pass after the schema addition.
- After Stage 2.2 Step 2, `cargo test --workspace` passes 943 tests, including the default-stack regression for block-scoped module initialization.
- Stage 2.2 Steps 3-8 added focused lowering coverage for module/initializer catalogs, function templates and thunks, symbol storage classes, compact type/trait metadata, subsystem semantic IDs, validation, and repeated-lowering determinism.
- Stage 2.3 added focused coverage for every function parameter pattern form, pattern-binding items (including at patterns and checked propagation), all assignment place operations, captured-cell and resource places, module binding/assignment/statement metadata, function body/result normalization, compile-time-only rejection, dangling pattern/place/item validation, and deterministic repeated lowering over the expanded arenas. After merging `main` and adding nominal-`move` transition coverage, the complete `cargo test --workspace` regression passes 989 tests.
- Stage 2.4 Step 1 replaced the syntax-only expression memo with an occurrence-aware key (syntax, owner, primary/contextual identity) and a single `classify_expression` dispatcher. Every syntax variant now maps to exactly one Stage 2.4 family, one explicit Stage 2.5/2.6 deferral (`Callable`, `Resource`, `Coroutine`), or a compile-time-only rejection; blocks allocate, memoize, and reuse their arena nodes, and loop bodies, match subjects/arms, products, spreads, access, indexing, logicals, and template interpolations are traversed recursively. A coverage classifier test constructs one representative per variant and is kept exhaustive by compile-time matches.
- Stage 2.4 Step 2 lowered scalars, ordinary names, and structural access. Integer/float payloads store the parsed value and checked scalar type with source-origin validation, strings and C strings decode once, names carry catalog storage/initialization/movement/singleton facts, callable values defer explicitly to Stage 2.5, and `CheckedAccess` copies into representation/product/slice/scalar nodes. Non-generic `const` bindings are runtime globals, so the symbol catalog now includes them and is populated before runtime expressions lower. Transition tests compare literals, names, and access against the checked side tables; the workspace regression passes 1000 tests.
- Stage 2.4 Step 3 lowered explicit, designated, spread, defaulted, and repeated products into owned plans. `LoweredProduct` separates source evaluation order (ordered steps, including expanded spread mappings and contextual defaults) from the final field layout, and replaying the steps reproduces the layout so later designators and spreads override earlier values exactly as the backend does. `LoweredRepeatedProduct` records the checked count and the single-element collapse.
- Stage 2.4 Step 4 finished blocks, `satisfies`, and coercion boundaries. `LoweredSatisfies` is an explicit wrapper, and snapshot-time validation now rejects duplicated block results, inconsistent expression-statement drop facts, and coercion source/target disagreements that are statically available.
- Stage 2.4 Step 5 lowered logicals, loops, and matches. `LoweredLogical` copies the checked `Bool` selection and resolves the `True` alternative; `LoweredLoop` records its body, result type, body-result drop requirement, fall-through fact, and nesting depth while break/continue items record the loop they exit; `LoweredMatch` records the checked subject type plus each arm's pattern, body, and bound symbols. Validation rejects orphaned or misdepth loop exits.
- Stage 2.4 Step 6 lowered index reads. `LoweredIndex` stores base/index in evaluation order plus the checked `Index` recipe (raw dispatch, owning trait, completed arguments, instantiated method type with masks/effects/resources, and operand temporary facts). Validation agrees dispatch arguments with operand types and cross-checks `MutateIndex` assignment dispatches against their lowered places.
- Stage 2.4 Step 7 lowered string templates and formatting selections. The checker retains a narrow side table with per-interpolation trait/method/value type and the standard formatter helper IDs; lowered templates preserve ordered decoded literals and left-to-right interpolations, and missing formatting metadata diagnoses during lowering instead of name-based backend discovery. The Stage 2.4 testing matrix is now covered end to end short of the final validation/coverage step.
- Stage 2.4 Step 8 removed the transitional `Pending` expression kind and completed validation: occurrence-lookup cardinality, logical true-index range, product step replay against the final layout, repeated-product count/collapse/result shape, literal payload validity, index dispatch completeness, interpolation trait/method ownership, and formatter helper catalog membership. A new ownership traversal from every initializer and function body reports arena nodes unreachable through typed edges while treating memoized sharing as intentional. A complete coverage fixture lowers every owned and deferred family concretely, consolidated transition tests compare payloads with their checked side tables, and normalized repeated-lowering snapshots now include formatter selections. **Stage 2.4 is complete; `cargo test --workspace` passes 1016 tests. Stage 2.5 is next.**
- Stage 2.5 Step 1 inventoried every runtime call route in the backend (juxtaposed chain, curried defaults, trait dispatch, intrinsic, generic direct, declared/global/extern, indirect closure, constructor, primitive macro, and compiler-helper routes) and mapped each to its checked inputs. The schema now has the eight explicit callable categories with no unknown fallback, typed callable targets, arena-backed calls and callable values with ordered argument/pass facts and ABI slots, call steps, closure-construction captures/access/ownership, `CallSubstitutions`, and `TraitEvidence` (explicit implementation, structural method, declared bound, and negative-implementation rejection data). `LoweredProgram::classify_call_route` mirrors the backend decision order, refines trait dispatch into selected/structural/declared-bound outcomes, and keeps local and captured callees indirect; `CallRoute::category` exhaustively maps every route onto a category. Validation checks the new arenas' catalog references, argument expressions/places/slots, step ranges, initialization symbols, closure captures, and evidence, and the ownership traversal reports unreachable calls and callable values. A decision-table test proves every route maps to a category and every category has exactly one target representation, and a source classification test covers all source-reachable routes. **Stage 2.5 Step 1 is complete; `cargo test --workspace` passes 1019 tests.**
- Stage 2.5 Step 2 replaced every callable-valued `Name`/`Access` deferral and the `Function` deferral with arena-backed callable values. Each carries an explicit target (direct function, constructor, extern, intrinsic, explicit/declared trait implementation, or structural method), checked function type, adapter kind, optional closure construction with catalog-ordered captures and per-capture access/ownership/drop facts, use-site type/effect substitutions, and trait evidence. Function value routes distinguish anonymous, nested, generic, declared, recursive, constructor, extern, and trait-method values, and `Stored` versus `Fresh` environments record whether a value is an existing closure or builds one. Function snapshots became two-pass and declaration catalogs now populate before expression lowering, so forward and recursive references resolve. **Stage 2.5 Step 2 is complete; `cargo test --workspace` passes 1021 tests. Step 3 (ordinary, direct, indirect, external, and intrinsic calls) is next.**
- Stage 2.5 Step 3 lowered ordinary (indirect closure), generic direct, external, and intrinsic calls. `Expression::Call` now dispatches through `lower_call`, which records each call's explicit target, callee occurrence (for indirect calls), checked call-site function type, ordered arguments with final ABI slots and pass modes, hidden effect resources, mutation/move markers, initialization checks, ordered steps, result type, and use-site substitutions. Mutation and non-`Copy` arguments become mutable place pointers or borrowed/materialized temporaries with drop facts, implicit thunk arguments record their thunk function, same-function generic recursion records the current environment, and extern C-string temporaries are explicit. Juxtaposed chains, curried defaults, trait dispatch, constructors, primitive macros, and calls whose arguments keep checked product plans or spread/designated elements stay explicitly deferred to Steps 4-6. **Stage 2.5 Step 3 is complete; `cargo test --workspace` passes 1024 tests. Step 4 (juxtaposed chains, curried defaults, and call arguments) is next.**
- Stage 2.5 Step 4 normalized juxtaposed call chains and call arguments. A complete juxtaposed call consumes its checked plan once at the outer call: inner chain calls are marked consumed and never lowered separately, the chain root is evaluated once as the callee (intrinsic roots keep their identity and evaluate no callee), and the plan's ordered arguments fill the flattened parameter slots. Call arguments reuse checked product plans: contextual defaults get their own occurrence keys and evaluate in final slot order, designated elements map to named slots, positional and named spreads expand to explicit mappings, non-product arguments against defaulted parameters fill slot 0 plus defaults, and variadic parameters accept extra slots. Implicit thunk arguments keep their function identity. Curried defaults remain deferred because source-level curried defaults are rejected during resolution. **Stage 2.5 Step 4 is complete; `cargo test --workspace` passes 1026 tests. Step 5 (constructor and compiler-provided call routes) is next.**
- Stage 2.5 Step 5 lowered constructor calls and compiler-provided routes. Constructor calls carry explicit `Constructor` targets with the selected symbol, type, and recursive-construction classification (managed-reference `Ref` construction is distinct from ordinary wrapping), evaluate their argument by value, and end with an invocation step. Constructor values keep their adapter, singletons remain values with their identity, and any surviving `c_string` primitive call normalizes to the same decoded owned C-string payload with interior-NUL validation. Compiler-helper identities stay recorded through checked formatter selections; Stage 4 completes the helper catalog. **Stage 2.5 Step 5 is complete; `cargo test --workspace` passes 1028 tests. Step 6 (trait evidence recipes) is next.**
- Stage 2.5 Step 6 lowered trait-dispatched calls and attached a single validated evidence recipe to every trait-dependent site: explicit implementation and method when checked arguments determine one, structural method with completed arguments, or a declared bound with the enclosing function's prerequisites for Stage 3. The same recipe covers trait calls, callable values, `Index` reads, `MutateIndex` assignments, and string-template display/debug interpolations, and validation checks each against the trait/method/implementation/function catalogs. No `Deferred(Callable)` expression remains. **Stage 2.5 Step 6 is complete; `cargo test --workspace` passes 1029 tests. Step 7 (validation, coverage, and transition checks) is next.**
- Stage 2.5 Step 7 completed validation, coverage, and transition checks. Validation rejects remaining callable deferrals and checks call target/callee ownership, target/evidence agreement, resource order against checked effect rows, exact argument-slot coverage (with a variadic fixed-prefix rule), substitution uniqueness, and index/mutation/interpolation evidence against their dispatches. Transition tests compare lowered calls with checked signatures, masks, resources, selected symbols, juxtaposed plans, and closure capture order/ownership, and mutation tests prove the new diagnostics fire. Representative CLI LLVM IR emission, object emission, and compile-and-run were exercised with the worktree standard library. **Stage 2.5 is complete; `cargo test --workspace` passes 1031 tests. Stage 2.6 (lower resources, reactive operations, and coroutines) is next.**
- Stage 2.6 Step 1 inventoried the backend's resource, reactive, and coroutine decisions and added the owned schema that will record them: `LoweredResourceProvider`/`LoweredResourceUse`/`LoweredWith`, `LoweredReactiveOperation`/`LoweredReactiveCallback`, and `LoweredCoroutinePlan`/`LoweredCoro`/`LoweredAwait` arenas with typed links to the expression, block, function, symbol, and call catalogs. `classify_expression` now returns an explicit `Stage26Route` for every remaining deferred expression (`ResourceUse`, `ResourceProvider`, `CoroutineCreation`, `AwaitChildCoroutine`, `AwaitTask`, `AwaitWait`) instead of an untyped deferral, `intrinsic_route` classifies every reactive and coroutine intrinsic with no ordinary fallback, and expression dispatch, arena-reference validation, loop-exit traversal, and the ownership reachability traversal already cover the new record families. The route table test proves every route maps to exactly one owned record family and every reactive/coroutine intrinsic has an explicit route. Concrete payloads land in Steps 2-6. `cargo fmt --all`, `cargo test -p staple-compiler` (92 unit + 487 compiler + 108 module tests), and `cargo check -p staple-compiler` pass. **Stage 2.6 Step 1 is complete; Step 2 (lower lexical resource providers and accesses) is next.**
- Stage 2.6 Step 2 lowered lexical resource providers and accesses. Every function body seeds its checked effect-row resources as stable `FunctionParameter` providers and every executable entry seeds its IO/reactive roots as `EntryParameter` providers; `with` evaluates its provider once, activates it only for the body, and records place-backed versus materialized storage, borrow/indirect facts, and `Reactive`/`Tasks`/ordinary scope exits. `resource` reads and resource assignment places now resolve to the nearest active provider by exact checked value type (the backend's rule) as explicit `LoweredResourceUse` records, with the backend's missing-provider diagnostic preserved. Validation checks provider origin/target agreement, same-owner nesting, use/provider type and indirectness agreement, `with` value/scope-exit agreement, and mutable-place legality; ownership treats function and entry providers as roots. Focused shadowing/storage/scope-exit/corruption tests and CLI resource, signal/reaction, and entry-reactive compile-run tests pass. `cargo test -p staple-compiler` passes 96 unit + 487 compiler + 108 module tests. **Stage 2.6 Step 2 is complete; Step 3 (bind ordered effect resources at use sites) is next.**
- Stage 2.6 Step 3 bound ordered effect resources at call sites. `LoweredCall` now stores `resource_bindings: Vec<LoweredResourceUseId>` instead of raw checked resources: each hidden requirement resolves to its selected provider in checked effect-row order, after visible arguments, with `BorrowedPointer` passing for mutable/non-`Copy` requirements (requiring addressable providers) and `Value` passing otherwise; missing, non-borrowable, and non-mutable providers keep the backend's source diagnostics. External, intrinsic, and constructor calls take no hidden resource ABI arguments and stay unbound (reactive intrinsics select their ambient provider in Step 4), while juxtaposed, generic-direct, trait, and indirect calls bind every ordered requirement, and unresolved generic effect templates keep no invented provider. Call steps keep callee/arguments/resource-lookup/invoke order, validation compares ordered bindings against the checked effect row, and the ownership traversal visits bindings. Focused order/scope/mutable/ABI-exclusion/template tests pass; `cargo test -p staple-compiler` passes 99 unit + 487 compiler + 108 module tests. **Stage 2.6 Step 3 is complete; Step 4 (lower signal, derived, and reactive lifecycle metadata) is next.**
- Stage 2.6 Step 4 attached reactive lifecycle metadata. Binding items own `SignalCreate` (module-global versus local-cell storage) or `DerivedCreate` (evaluator thunk, checked callback type, ordered captures, resource-free requirement); name reads own `SignalRead`/`DerivedRead`; assignments own `SignalNotify`; and the five reactive intrinsics own explicit operations (`Scope`, `Snapshot`, `Reaction`/`Until` with their thunk-or-callable callback, ordered callback resources, and selected ambient `Reactive` provider, and `Batch`). Validation cross-checks attachments, callback identity/captures/resources, storage classification, and `until` purity, and the ownership traversal visits reactive operations. Focused signal/derived/scope/reaction/batch/until/snapshot and corruption tests plus CLI reactive compile-run tests pass; `cargo test -p staple-compiler` passes 102 unit + 487 compiler + 108 module tests. **Stage 2.6 Step 4 is complete; Step 5 (own coroutine plans and body/thunk relationships) is next.**
- Stage 2.6 Step 5 moved every coroutine plan into an owned lowered record. Each plan links its body syntax and block, owning implicit thunk, ordered captures, checked result/deferred effects, resume count, ordered frame bindings, awaited result types, and wait/`until` cancellation states, cross-checked against the checked `Coroutine{E} T` thunk signature. Validation checks capture agreement, body/body-syntax agreement with the thunk, frame-binding ownership without capture overlap, resume/await cardinality, and state ranges; plans seed the ownership traversal through their thunk catalog entries. Nested `coro` bodies remain separate plans, and non-statement-position `await` stays a checking diagnostic. Focused plan/scanner/nesting/cancellation tests and CLI coroutine compile-run tests pass; `cargo test -p staple-compiler` passes 106 unit + 487 compiler + 108 module tests. **Stage 2.6 Step 5 is complete; Step 6 (lower `coro` creation and ordered `await` operations) is next.**
- Stage 2.6 Step 6 lowered `coro` creation and `await`. Plans now pre-exist body lowering so creation and await sites resolve during body lowering; `coro` links its plan and capture environment, and each `await` records its operand, checked result type, owning plan, one-based resume state in source order, child/`Task`/`Wait` kind, child result, deferred child-resource bindings acquired at activation, and `until` classification. Validation requires one-to-one await/resume mapping, awaited-type agreement, and classification against the plan's wait/`until` state lists. No Stage 2.6 expression deferral remains. Focused creation/await-kind/child-resource/corruption tests and CLI coroutine/task compile-run tests pass; `cargo test -p staple-compiler` passes 110 unit + 487 compiler + 108 module tests. **Stage 2.6 Step 6 is complete; Step 7 (close validation and transition coverage) is next.**
- Stage 2.6 Step 7 closed validation and transition coverage. Any remaining `Deferred(Resource)`, `Deferred(Coroutine)`, or `Stage26Deferred` expression now diagnoses as unlowered, so no Stage 2.6 deferral can pass silently. The Step 2-6 checks together validate provider scope/use consistency, checked effect-row resource ordering, reactive callback and evaluator identity, coroutine body/thunk ownership and capture order, await-state sequences with cancellation classifications, and every arena/catalog link. A new transition test compares every provider target/resource, resource use, entry resource, reactive thunk/capture link, signal/derived classification and evaluator, coroutine plan field, and await kind against `TypedModule`, and normalized repeated-lowering snapshots now include all Stage 2.6 arenas with byte-for-byte determinism. `cargo fmt --all -- --check`, `cargo check --workspace`, `cargo test --workspace` (1054 tests including CLI compile/run, object-emission, and LLVM-verification paths), and `git diff --check` pass. **Stage 2.6 is complete; Stage 2.7 (complete validation, coverage, and transition checks) is next.**
- Stage 2.7 completed validation, source coverage, and transition checks. `validate_arena_identity` checks dense arena/catalog insertion positions and every occurrence and coroutine-plan lookup against its arena. The ownership traversal now attributes every reachable node to exactly one runtime owner: function and entry providers, initializers, functions, and coroutine plans seed it with their owner, expressions must agree with their occurrence key, and coroutine plans always belong to their body thunk even when linked through another function's `coro`/`await`, so cross-owner references and unreachable nodes both diagnose. `validate_function_body_ownership` proves body blocks and parameter patterns have one owner, `validate_concrete_metadata` rejects `Inferred`/`Error` placeholders in runtime types while keeping declared parameters and diverged `Never` values legal, and `validate_capture_consistency` checks function, callback, and plan captures against symbol storage and ownership. A new `SourceCoverage` traversal mirrors the lowering walk over the checked program and proves every declared function and implicit thunk has exactly one lowered template with a matching body syntax, every initializer owns exactly its runtime source items in order, and every runtime expression, pattern, and assignment place has a lowered counterpart under the same owner; `Lowerer::lower` runs it after internal validation, so the CLI and codegen suites exercise it. New tests cover a fixture spanning every expression family plus resources, reactive operations, implicit thunks, captures, and coroutines; corrupted ownership, lookups, metadata, coverage, and invented templates; and a transition comparison of every `Primary` expression's checked type, effects, coercion, and moved symbols plus every function's signature and capture order. `cargo fmt --all -- --check`, `cargo check --workspace`, `cargo test --workspace` (1057 tests including CLI compile/run, object-emission, and LLVM-verification paths), and `git diff --check` pass. **Stage 2 is complete; Stage 3 (structural instance keys and the specialization worklist) is next.**

### Remaining Verification

- Add lowering tests proving:
  - repeated concrete uses deduplicate;
  - nested and same-specialization recursive generic calls terminate;
  - result-only and effect substitutions are retained;
  - outer substitutions affecting captures or bodies create distinct instances;
  - generic closures, companion methods, defaults, constructors, and trait calls contain explicit targets and evidence;
  - unused generic bodies are absent;
  - instance ordering and names are deterministic.
- Add validator tests for unresolved parameters, missing evidence, dangling references, and non-concrete emitted instances.
- Test implicit artifact discovery for formatting, drop/debug/index methods, structural traits, reactive thunks, and coroutine resume/cleanup.
- Preserve LLVM assertions for specialized direct calls, closure environments, effects/resources, ownership cleanup, structural traits, reactive behavior, and coroutine cancellation.
- Run the complete workspace suite on LLVM 21, including CLI compile/run, module tests, examples, object emission, and LLVM verification.
- Compare representative LLVM before and after the refactor to detect ABI changes or duplicate instances.

## Assumptions

- The implementation targets the current working tree, including its existing uncommitted language changes.
- `LoweredModule` is compiler-facing and has no stable serialization or binary-compatibility promise.
- Emitted function instances are fully concrete; generic templates exist only inside lowering.
- Scheme abstraction, stored polymorphic values, erased calls, dictionaries, descriptors, runtime-sized layouts, and related tooling remain for the later feature update.
- Existing language behavior and concrete ABI are unchanged by this preparatory update.
