# Stage 2.2 Plan: Snapshot Modules, Functions, Symbols, and Type Metadata

## Goal

Populate `LoweredProgram` with deterministic, owned declaration catalogs before lowering runtime bodies. After this work, later lowering stages can resolve a module, function, symbol, type, trait, or runtime subsystem identity through lowered data instead of reaching back into `TypedModule` side tables.

This substage does not lower expression, pattern, or runtime-item bodies. It records their source ownership and ordered roots so Stages 2.3-2.6 can populate the existing arenas without rediscovering declarations.

## Completion Criteria

- Every source module has one lowered module record, ordered by initialization order and retaining its semantic `ModuleId`.
- Every resolved function and implicit thunk has one lowered function-template record keyed by `FunctionId`.
- Every runtime symbol has one lowered symbol record keyed by `SymbolId`, including checked type, owner, module, storage classification, and declaration origin.
- Types, traits, trait methods, implementations, standard semantic IDs, and subsystem type IDs needed by later stages are owned by `LoweredProgram`.
- Every module initializer records its ordered runtime-item sources; its lowered body remains empty until Stage 2.3.
- Catalog construction and validation are deterministic and do not depend on `HashMap` iteration order.
- The LLVM backend continues to use only the private legacy bridge during this substage.

## Representation Additions

Add the following private or `pub(crate)` records in `lower.rs`, splitting them into a sibling module if the file becomes unwieldy:

- `LoweredModuleInfo`
  - semantic `ModuleId`, `Origin`, qualified name, optional parent, companion flag;
  - initialization index and executable-entry flag;
  - initializer ID.
- `LoweredFunction`
  - existing semantic `FunctionId` and origin;
  - name, owning module, checked `CheckedFunctionType`, checked trait bounds;
  - parameter style and parameter symbols in source/destructuring order;
  - ordered captures with ownership facts;
  - source body syntax ID and optional lowered body ID;
  - classification: declared function, implicit thunk, derived evaluator, coroutine body thunk, extern, or intrinsic-backed declaration.
- `LoweredCapture`
  - symbol ID plus borrowed/owned and shared-cell requirements available before expression lowering.
- `LoweredSymbol`
  - semantic `SymbolId`, declaration origin, owner function, module, checked type;
  - storage classification and orthogonal flags for initialization checking, derivation, signal behavior, parameter mutation/move, and capture-cell use;
  - optional function, constructor, singleton, intrinsic, or external target identity.
- `SymbolStorage`
  - immutable SSA value, mutable cell, global storage, function binding, derived binding, signal, captured cell, or external symbol.
- `LoweredTypeMetadata`
  - `TypeId`, origin, name, module, declaration kind, checked parameter metadata;
  - builtin and recursive-construction classification;
  - an owned checked representation template where one exists.
- `LoweredTraitMetadata`, `LoweredTraitMethodMetadata`, and `LoweredTraitImplementationMetadata`
  - semantic IDs, origins, owning modules, checked parameter templates, functional dependencies, method signatures/defaults, checked implementation arguments/bounds, negative flag, and selected method functions.
- `LoweredSemanticIds`
  - optional IDs for standard traits (`Copy`, `Drop`, `Debug`, `Display`, `Index`, `MutateIndex`, `IntoIterator`, `Iterator`, and any prerequisite used by later lowering);
  - optional IDs for IO, reactive, coroutine, task/completion, scheduler, wait/resolver, and related runtime types;
  - string representation and entry-reactive requirement.
- `RuntimeItemSource`
  - source `Origin` and a small runtime-item category only; no cloned AST node.
  - Each `LoweredInitializer` owns these entries in exact source order until Stage 2.3 replaces them with `ItemId`s in its body block.

Catalogs should use insertion-ordered vectors plus semantic-ID-to-arena-ID lookup maps. Maps are for lookup only; observable traversal always uses the vectors.

## Implementation Sequence

### Step 1 - Finalize Catalog Invariants and Keys (Done)

- Extend `LoweredProgram` with module, symbol, type, trait, trait-method, and trait-implementation catalogs.
- Add lookup maps from semantic IDs to arena/catalog IDs.
- Keep semantic IDs distinct from arena IDs; never assume `FunctionId.0`, `SymbolId.0`, or `TypeId.0` is a vector index.
- Define duplicate insertion as a lowering diagnostic rather than overwriting an earlier record.
- Add accessors used only by lowering and validation; keep public `LoweredModule` opaque.

Gate: empty catalogs validate, duplicate semantic IDs are rejected, and lookup maps agree with ordered catalog entries.

After this step, update `TYPED_LOWERING_PLAN.md` with general progress and `STAGE_2_LOWERING_BREAKDOWN.md` with the completed details.

Completed:

- Added distinct typed catalog IDs for modules, functions, symbols, types, traits, trait methods, and trait implementations.
- Added a reusable insertion-ordered `Catalog` whose semantic lookup map is not used for traversal.
- Added duplicate-ID diagnostics that preserve the first catalog entry.
- Added bidirectional validation for ordered entries and lookup indexes, including dangling and mismatched map entries.
- Extended `LoweredProgram` with all Step 1 catalogs while retaining the Stage 2.1 node arenas.
- Verified with formatting, workspace checking, five focused lowering tests, and `git diff --check`.

### Step 2 - Add Deterministic Typed-Module Inventory APIs (Done)

Add narrow `pub(crate)` inventory methods rather than exposing whole side tables:

- `ResolvedModule`: symbols in ascending `SymbolId`, type parameters in ascending ID, types in ascending `TypeId`, traits in ascending `TraitId`, and trait methods in ascending `TraitMethodId`.
- `TypedModule`: implicit thunks in ascending `FunctionId`, checked trait implementations in declaration order, derived evaluator relationships, standard trait/type IDs, and owned checked type/trait templates.
- Preserve current source order for declared functions; merge implicit thunks by stable `FunctionId` ordering after declared functions unless their recorded source owner provides a stricter order.
- Add checked metadata during type checking where it is currently temporary. Preserve checked trait implementation records now; define compact, non-expanding canonical type-representation templates with the type catalog in Step 6 rather than retaining recursively expanded `CheckedType` trees. Do not reconstruct semantic decisions from source `Type` syntax in lowering.

Gate: repeated inventory calls produce identical sequences, and tests deliberately insert metadata through differently ordered hash maps without changing output.

After this step, update both plan files.

Completed:

- Added narrow resolver inventories ordered by `SymbolId`, `TypeParameterId`, `TypeId`, `TraitId`, and `TraitMethodId`.
- Added stable type-checker inventories for implicit thunks, derived evaluators, checked trait method/parameter/dependency metadata, and checked implementations.
- Retained the type checker's selected standard trait and runtime type IDs in one compiler-facing semantic-ID snapshot.
- Kept declaration-ordered vectors authoritative where source order matters and sorted all map-backed inventories before returning them.
- Added focused coverage that checks inventory completeness and strict semantic-ID ordering on a successfully checked standard-library program.
- Confirmed through transition testing that eagerly retaining expanded `CheckedType` representations overflows normal stacks for recursive standard-library metadata; Step 6 will introduce compact templates that preserve named type references instead.
- Boxed the private legacy `TypedModule` payload in `LoweredModule` after the larger transitional owner exposed stack pressure in block-scoped module compilation; this is representation-private and leaves backend behavior unchanged.
- Verified the default-stack regression directly and passed the complete workspace suite (943 tests), workspace check, formatting check, and diff check.

### Step 3 - Snapshot Modules and Initializer Roots (Done)

- Traverse `Program::initialization_order()` and create one `LoweredModuleInfo` per module.
- Validate that initialization order contains each loaded module exactly once; diagnose missing, duplicate, or unknown IDs.
- Preserve qualified name, parent, companion status, executable entry, and source origin. For file modules without a declaration node, use the module syntax origin.
- Allocate one empty `LoweredBlock` and one `LoweredInitializer` per module.
- Scan each module's top-level items and record only runtime categories (`Binding`, `PatternBinding`, `Assignment`, `Return`, `Break`, `Continue`, and `Expression`) as `RuntimeItemSource` entries in source order. Declaration-only items remain outside the initializer.
- Record entry IO/reactive resource requirements as initializer metadata, not synthetic runtime AST nodes.

Gate: single-module, multi-module, dependency, companion, and declaration-only programs produce stable module and initializer catalogs matching current initialization order.

After this step, update both plan files.

Completed:

- Traversed `Program::initialization_order()` and inserted one `LoweredModuleInfo` per module, preserving semantic `ModuleId`, qualified name, parent, companion flag, initialization index, executable-entry flag, and the module's `InitializerId`.
- Added `validate_initialization_order`, which diagnoses unknown, duplicate, and missing module IDs against the loaded module table before catalogs are built.
- Module origins use the declaration syntax when one exists and the module syntax origin for file modules without a `mod` declaration.
- Allocated one empty `LoweredBlock` and one `LoweredInitializer` per module; initializers record `RuntimeItemSource` entries for `Binding`, `PatternBinding`, `Assignment`, `Return`, `Break`, `Continue`, and `Expression` items in exact source order without cloning AST nodes.
- Entry initializers record IO and reactive `CheckedResource` requirements as metadata (`LoweredEntryResource`) instead of synthetic runtime AST nodes.
- Added focused tests covering multi-module initialization order, declaration-only modules, source-order runtime categories, companion parentage, file-module declaration origins, entry IO metadata, repeated-lowering stability, and the unknown/duplicate/missing order diagnostics.
- Verified with formatting, workspace checking, focused lowering tests, `cargo test --workspace` (951 tests), and `git diff --check`.

### Step 4 - Snapshot Function Templates and Implicit Thunks

- Insert all resolved functions in their stable resolver order, then all implicit thunks in stable order.
- Copy checked signature and bounds; failure to find either required checked data is a lowering diagnostic at the function origin.
- Recursively collect parameter symbols from the resolved parameter pattern in source order.
- Record body origin and owning module without cloning the body expression.
- Copy captures in resolver order and attach `is_borrowed_capture`, `is_non_owning_symbol`, and captured-cell requirements.
- Classify implicit thunks by their relationships: derived evaluator, coroutine body plan, resource/reactive helper, or ordinary implicit thunk. A thunk may retain explicit flags if classifications overlap.
- Record function-binding symbols and extern/intrinsic classification without assigning specialization instances.

Gate: every `FunctionId` from declared functions and implicit thunks appears exactly once; signatures, parameters, captures, ownership flags, and classifications match transition-time comparisons with `TypedModule`.

After this step, update both plan files.

### Step 5 - Snapshot Symbols and Storage Classification

- Enumerate declared runtime symbols deterministically from resolver-owned declaration records, supplementing compiler-created symbols referenced by functions, captures, constructors, singleton values, intrinsics, and implicit thunks.
- Copy declaration origin, module, owner, and declared checked type. Missing types are diagnostics except for explicitly compile-time-only symbols, which must not enter this catalog.
- Compute storage using a documented precedence so classification is stable:
  1. external symbol;
  2. function/constructor/singleton binding;
  3. derived binding;
  4. signal;
  5. captured mutable/derived cell;
  6. module/global storage;
  7. mutable local cell;
  8. immutable SSA value.
- Keep mutation, move, initialization-state, and capture ownership as independent flags; do not overload the storage enum with these facts.
- Validate every parameter and capture symbol referenced by a function and every runtime initializer symbol against the catalog.

Gate: fixtures covering globals, locals, mutable parameters, owned and borrowed captures, functions, externs, signals, derived bindings, constructors, and singleton values receive the expected unique classification.

After this step, update both plan files.

### Step 6 - Snapshot Type and Trait Metadata

- Insert types by ascending semantic ID while retaining source origin and module identity.
- Store checked type parameters, builtin identity, recursive-construction strategy, declaration kind, and checked representation template.
- Insert traits and methods by semantic ID, preserving declared method order inside each trait.
- Copy checked trait parameter templates, functional dependencies, method types/default functions, and checked implementations in declaration order.
- Preserve generic parameters and checked bounds structurally; do not stringify them or resolve implementation selection here.
- Keep source declarations out of `LoweredProgram` once their semantic contents have been copied.

Gate: generic types, opaque/distinct types, multi-parameter traits, functional dependencies, defaults, negative implementations, and structural-trait prerequisites can be inspected using lowered metadata alone.

After this step, update both plan files.

### Step 7 - Snapshot Standard and Runtime Subsystem Identities

- Populate `LoweredSemanticIds` from type-checker-owned selections rather than looking up names again.
- Include all standard traits and runtime types currently consulted by code generation, even when absent in `no_prelude` or library-only programs.
- Copy string representation, executable entry, entry reactive requirement, and canonical IO/reactive resource types.
- Validate that present semantic IDs refer to corresponding type or trait catalog records.

Gate: ordinary, `no_prelude`, library-only, reactive, resource, and coroutine fixtures all lower without name-based semantic rediscovery.

After this step, update both plan files.

### Step 8 - Complete Validation and Transition Comparisons

Expand `LoweredProgram::validate` to check:

- unique module, function, symbol, type, trait, and trait-method semantic IDs;
- lookup-map/catalog agreement;
- valid module parents and complete initializer order;
- function owner, parameter, capture, and function-binding symbol references;
- symbol owner/module/type and constructor/singleton/intrinsic targets;
- type/trait/method/implementation cross-references;
- standard/runtime IDs resolving to the correct catalog family;
- exactly one initializer per module and stable runtime-item source ordering.

Add test-only transition comparisons that build the catalog twice and compare normalized snapshots, then compare selected lowered records with their `TypedModule` sources. Diagnostics must use the nearest stored `Origin`; compiler-only inconsistencies use `Span::Compiler` only when no source origin exists.

Gate: all Stage 2.2 unit and fixture tests pass, followed by the complete regression command set.

After this step, mark Stage 2.2 complete in both plan files and identify Stage 2.3 as next.

## Testing Matrix

- Catalog mechanics: duplicate IDs, missing lookup entries, dangling cross-references, deterministic insertion and iteration.
- Modules: empty/declaration-only source, inline and file modules, companions, package dependencies, cycles with valid initialization order, executable versus library entry.
- Functions: named, local, generic, nested closures, overloads, externs, intrinsics, implicit argument thunks, derived evaluators, coroutine body thunks.
- Symbols: immutable/mutable locals, globals, parameters, captured cells, owned/borrowed captures, signal/derived bindings, constructors, singleton values, externs.
- Types and traits: aliases, constructors, opaque/distinct and recursive types, checked representations, generic/effect parameters, trait defaults, functional dependencies, negative and generic implementations.
- Runtime identities: standard library present/absent, IO entry, reactive entry, resources, coroutine/task/completion/scheduler types.
- Determinism: lower the same checked module repeatedly and compare a normalized catalog snapshot byte-for-byte.

Run after every completed step:

```text
cargo fmt --all -- --check
cargo check --workspace
cargo test -p staple-compiler lower::tests
git diff --check
```

Run at the final Stage 2.2 gate:

```text
cargo test --workspace
```

## Risks and Decisions

- Existing `HashMap`-backed side tables cannot define catalog order. Every new inventory API must sort by semantic ID or preserve an already-defined source/declaration order.
- `symbol_types` is demand-populated. Symbol lowering must use `declared_type_of_symbol` and add checked declaration metadata where that fallback is insufficient; it must not silently omit unused declarations.
- Type representations and checked trait implementation data currently span private type-checker tables. Copying them into explicit owned records is part of this substage; exposing the entire `TypedModule` is not.
- Storage categories overlap conceptually. The precedence above determines the primary category, while independent flags preserve facts needed by ownership and initialization lowering.
- Runtime item bodies remain intentionally unlowered. Stage 2.2 records only their ordered origins/categories and allocates initializer roots; Stage 2.3 creates `LoweredItem` and `LoweredPattern` nodes.
- No function instance IDs, specialization worklist, helper catalog, or LLVM migration belongs in this plan.

## Deliverables

- Extended lowered catalog schema and deterministic lookup infrastructure.
- Narrow deterministic inventory APIs on resolver/type-checker state.
- Populated module, initializer-root, function, symbol, type, trait, and subsystem metadata.
- Comprehensive catalog validation and transition comparison tests.
- Updated `TYPED_LOWERING_PLAN.md` and `STAGE_2_LOWERING_BREAKDOWN.md` after every completed step.
