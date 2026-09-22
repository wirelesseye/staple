# Add a Typed Lowering and Specialization Phase

## Summary

Introduce a behavior-preserving compiler phase:

```text
resolve -> type check/ownership -> lower and specialize -> LLVM
```

LLVM generation will consume lowered IR and will no longer infer types, select trait implementations, or discover generic instances. This update does not add polymorphic values, erased dictionaries, runtime descriptors, or ABI changes.

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

### Stage 1 - Define the Lowered IR and Pipeline Boundary

- Add `lower.rs` with arena-backed lowered modules, expressions, patterns, initializers, callable targets, function instances, trait evidence, closure construction, ownership facts, and helper requirements.
- Preserve source spans and syntax IDs for diagnostics.
- Make `LoweredModule` own all semantic information required by LLVM generation; it must not retain a dependency on the source AST.
- Update the CLI and test helpers to construct a `LoweredModule`, initially allowing code generation migration to proceed construct by construct within the branch.
- Add an IR validator for dangling arena references, missing types, unresolved targets, incomplete evidence, and invalid instance references.

### Stage 2 - Lower Existing Typed Programs Completely

- Lower every current expression, pattern, binding, initializer, implicit thunk, and coroutine plan.
- Record concrete type/effect information, coercions, accesses, selected symbols, storage requirements, ownership operations, and ordered resource arguments directly on lowered nodes.
- Represent calls explicitly as:
  - direct function-instance calls;
  - indirect concrete-closure calls;
  - external calls;
  - intrinsics;
  - concrete trait implementation calls;
  - structural trait calls.
- Represent closure creation with its code target, capture order, capture ownership, environment inputs, and required concrete adapter.
- Carry enough cleanup and ownership metadata for the backend to preserve moves, borrows, drops, early returns, propagation, and cancellation without querying `TypedModule`.

> **Complex stage:** The AST and backend support many specialized constructs, including defaults, reactive bindings, structural indexing, ownership cleanup, and coroutines. This stage may need separate breakdown plans by expression family and runtime subsystem during implementation.

### Stage 3 - Add Structural Instance Keys and the Specialization Worklist

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

### Stage 4 - Record Compiler-Generated Artifacts Before LLVM

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

### Stage 5 - Migrate LLVM Generation to Lowered IR

- Predeclare all functions, adapters, and helpers from the lowered catalog.
- Emit bodies in deterministic instance order.
- Replace AST traversal and `TypedModule` side-table queries with lowered-node traversal.
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

### Stage 6 - Remove Transitional Code and Document the Boundary

- Remove obsolete `TypedModule` accessors used only by the old backend path while retaining APIs needed by diagnostics, tooling, and lowering.
- Add module-level documentation describing phase responsibilities and invariants.
- Confirm that lowering failures produce source-based diagnostics rather than backend panics.
- Verify that concrete closure, resource, coroutine, FFI, and ownership ABIs are unchanged.

## Test Plan

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
