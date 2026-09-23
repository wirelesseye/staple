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
- **Stage 2 is in progress.** Stage 2.1, all of Stage 2.2, and Stage 2.3 are complete, and Stage 2.4 Step 1 has landed. Lowering now owns validated declaration catalogs plus lowered patterns, assignment places, runtime items, function bodies, and module initializer bodies, with deterministic validation and transition comparisons. Stage 2.4 (lower ordinary expressions and control flow) is in progress: every `Expression` variant now has one exhaustive owned/deferred/rejected decision, expression identity is occurrence-aware, and block/function/initializer ownership is explicit. Stage 5 still removes the temporary legacy payload after migrating the backend.
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

### Stage 2 - Lower Existing Typed Programs Completely (In Progress: 2.1-2.3 Done)

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
