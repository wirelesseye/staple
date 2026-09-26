# Stage 4 Breakdown: Record Compiler-Generated Artifacts Before LLVM

## Status and Goal

**Status:** Not started. Stage 3 is complete at `1b347a8`: `LoweredProgram` owns the reachable `LoweredFunctionInstance` graph with materialized concrete bodies, typed constructor-adapter and structural-method artifact *requests* (keys only, no bodies), and a list of unresolved `LoweredCompilerHelperRequest`s. The legacy backend still emits every generated function itself, discovering most of them deep inside LLVM emission through `TypedModule` queries, name lookups, `Debug`-string cache keys, and syntax-ID-keyed names.

Stage 4 makes the lowered catalog **closed**: before LLVM runs, every function the backend will emit exists as a lowered source-function instance or as a typed, keyed, deduplicated generated artifact with an owned lowered plan, and every reference from a body or from another artifact resolves to one catalog entry. Stage 4 does not switch the backend to the catalog (that is Stage 5), it does not change the emitted ABI, and it adds no new source syntax or runtime polymorphism. The legacy backend and the `LoweredModule::typed` bridge stay operational; the transition comparison in Stage 4.7 proves the new catalog covers everything legacy emission generates.

Line references below are against `1b347a8` and will drift; re-locate them by function name.

## Artifact Contract

- **Two catalog families only.** A function the backend emits is either a `FunctionInstanceId` (source template + substitution, Stage 3) or an artifact ordinal in the `SpecializationCatalog` artifact family. No third, backend-private category may remain. The existing `CompilerHelper` callable category must be resolved to one of the two; `helper_requests` must be empty (or removed) when Stage 4 finishes.
- **Typed structural keys.** Every artifact family gets a variant in `ArtifactRequestKey` built from semantic IDs and `CanonicalType`/`CanonicalFunctionType` values (Stage 3.1 converters). No key may contain a `Debug` string, a `DefaultHasher` value, a `SyntaxId`, or a display name. Artifacts whose identity is per-site (reactive runners, coroutine resume/cleanup) are keyed by the owning instance or initializer plus the lowered site ID, never by source `SyntaxId` alone: a generic template instantiated twice must produce two artifacts. Bump `SPECIALIZATION_KEY_ENCODING_VERSION` when the encoding changes.
- **Owned plans, not source synthesis.** Each artifact carries a typed lowered plan (`LoweredArtifactPlan`, one variant per family) recording exactly the decisions the backend currently makes while emitting it: concrete types, field/alternative order, selected callees (as `FunctionInstanceId` or artifact ordinal), layout-relevant facts, and ownership/cleanup facts. Do not fabricate source AST or fake `Expression` nodes to express a generated body. Target-specific LLVM layout (sizes, alignment, struct types) stays in the backend; the plan records the concrete `CheckedType`s the backend lays out.
- **Closure under dependencies.** Artifact plans can request source-function instances (for example a structural `Debug` body calling a generic `Debug` implementation for a field type, buffer clone selecting a generic `Clone` implementation) and other artifacts (nested drop glue, finalizers, nested structural methods). The catalog must reach a fixed point in which every such request is interned, every newly interned instance is materialized with the Stage 3.4 machinery, and its body is scanned in turn.
- **Determinism.** Artifact ordinals are assigned in first-discovery order over a deterministic traversal: Stage 3 emission order first, then fixed-point rounds, each round scanning owners in catalog order and sites in lowered evaluation order. Repeated lowering yields byte-identical catalog snapshots and planned names.
- **No backend discovery.** A Stage 4 artifact is complete only when its plan names every callee and nested artifact the backend would otherwise look up through `TypedModule::{trait_impl_method, structural_trait_method, instantiated_trait_method_type, drop_method_for, coroutine_plan, implicit_thunk_for, type_needs_drop, is_copy_type}`, `resolved().standard_trait(..)`, `standard_function_named`, or `standard_function_name_matches`.

## Hidden Discovery Paths to Relocate

This is the starting inventory; Stage 4.1 must verify and complete it (every `add_function` and every function-valued cache in `codegen.rs`).

| Backend path | Current key / name | Hidden dependencies discovered during emission | Target family |
| --- | --- | --- | --- |
| `ensure_constructor_adapter` (`codegen.rs:982`) | `(SymbolId, Debug fn type)` | constructor layout | `ConstructorAdapter` (key exists; needs plan) |
| `structural_trait_method_code` (`3739`) and `compile_structural_*_body` (`3839`–`4738`) | `(StructuralTraitMethod, Debug args)` | Debug: `standard_trait("Debug")` per field/alternative via `trait_method_code`, `Formatter.write` by name (`compile_formatter_write_literal` `3984`); DerefIndex/DerefMutateIndex: `standard_trait_id("Index"/"MutateIndex")` (`4384`, `4438`) + `trait_method_code`; Next: `IterStep` alternative matching | `StructuralMethod` (key exists; needs plan + nested requests) |
| `compile_string_template` (`4037`) | name lookup `formatter_new`/`formatter_finish`, `formatter_write` for literal parts | `Formatter.write` edge is not recorded by the worklist (only constructor/finish are `LoweredInstanceDependencyKind`s) | record a `FormattingWrite` instance edge |
| `compile_drop_value` (`1525`) and `compile_conditional_drop` (`1710`) | inline, recursive over the type | `TypedModule::drop_method_for` (exact-argument `Drop` impl, emitted only if already in `self.functions`), coroutine `cleanup`, scheduler/wait/resolver/completion-token runtime release, CString `free`, product/sum/distinct recursion via `type_needs_drop` | `DropGlue` per concrete type |
| `ensure_gc_finalizer` (`6017`) | `Debug` payload + `DefaultHasher` | payload drop glue | `GcFinalizer::Payload` |
| `ensure_cell_finalizer` (`6057`) | `"cell:" + Debug` + hash | cell value drop glue | `GcFinalizer::Cell` |
| `ensure_closure_finalizer` (`11174`) | `closure:{FunctionId}:{Debug capture types}` + hash | capture drop glue under `active_type_substitutions` | `GcFinalizer::ClosureEnvironment` keyed by the instance and capture layout |
| `ensure_buffer_finalizer` (`13444`) | `"buffer:" + Debug` + hash | element drop glue | `GcFinalizer::Buffer` |
| `compile_buffer_clone` (`13136`) via intrinsic `BufferClone` | none | `standard_trait("Clone")` by name + `trait_method_code` for the element, buffer finalizer | record `Clone` evidence and finalizer on the intrinsic site (Clone trait ID is not in `LoweredSemanticIds` yet) |
| `ensure_coroutine_codes` (`9414`) | `__staple_coro_{body SyntaxId}` | `typed_module.coroutine_plan`, `implicit_thunk_for`, `coroutine_frame_layout`, closure finalizers for thunk environments (`9756`, `9912`), `llvm.trap` | `CoroutineResume` + `CoroutineCleanup` keyed by coroutine body instance |
| Reaction runner (`~7762`) | `__staple_reaction_runner_{call SyntaxId}` | callback call shape, resources | `ReactionRunner` keyed by owner + reactive-operation site |
| `emit_until_runner` (`8085`) | `__staple_until_runner_{call SyntaxId}` (reuses by name) | predicate closure type | `UntilRunner` keyed by owner + site |
| `compile_derived_create` runner (`8204`/`8265`) | `__staple_derived_runner_{evaluator SymbolId}` | evaluator call shape | `DerivedRunner` keyed by owner + site |
| Extern closure adapters (`declare_external_functions` `577`, stored in `closure_codes`) | `__staple_extern_{name}` | extern callable type | `ExternAdapter` keyed by `SymbolId` + canonical callable type |
| `build_utf8_validator` (`8765`), runtime `.ll` installs (`477`–`548`), lazily declared libc/LLVM functions (`free`, `memcmp`, `snprintf`, `strlen`, `memchr`, `llvm.trap`, `__staple_gc_*`, coroutine/reactive runtime entry points via `coroutine_runtime_fn`/`build_reactive_runtime_call`) | by symbol name | none (fixed runtime surface) | `RuntimeHelper` requirement set recorded per lowered operation |

Known latent defects this inventory exposes (fix only as far as Stage 4 needs; do not change legacy emission): per-syntax-ID runner and coroutine names alias across generic instances, and `compile_drop_value` silently depends on the `Drop` method already being an eager root.

## Stage 4.1 - Inventory, Schema, and Key Families

- Freeze the route matrix above in a Stage 4.1 notes section: read every `add_function`, every `get_function(..).unwrap_or_else(add_function)`, every function-valued `HashMap` on `ModuleEmitter`, and every `trait_method_code`/`ensure_function_specialization` call reached from inside a generated body. For each, record trigger, current key, hidden dependencies, the owned lowered record that supplies its inputs, and the target artifact family. Add a negative matrix for what stays backend-local (pure LLVM intrinsics and target layout computations).
- Extend `ArtifactRequestKey` with the new families (`DropGlue`, `GcFinalizer { Payload | Cell | ClosureEnvironment | Buffer }`, `CoroutineResume`/`CoroutineCleanup` (or one `CoroutineCodes` pair key), `ReactionRunner`, `UntilRunner`, `DerivedRunner`, `ExternAdapter`) and decide whether `RuntimeHelper` is an artifact family or a separate requirement set (recommendation: a separate ordered `LoweredRuntimeRequirements` set, since those symbols have fixed names and no bodies Stage 4 plans). Extend the canonical encoding, family tags, `planned_names` (stable, family-prefixed, ordinal-based), and name-collision checks. Bump the encoding version.
- Add `LoweredArtifactDependencyKind` variants and an artifact-owned dependency list so edges can originate from an artifact (`LoweredArtifactRequestRoot::Artifact { artifact, kind, origin }`), and an instance-request variant `LoweredInstanceRequest::Artifact { .. }` for instances first requested by a generated body.
- Add missing semantic IDs to `LoweredSemanticIds` from checker-owned selections (at minimum the `Clone` trait and any runtime types `compile_drop_value` special-cases that are not already present). Add a `FormattingWrite` instance dependency kind and record the `Formatter.write` edge for template literal parts.
- Add the `LoweredArtifactPlan` enum with one placeholder variant per family and exhaustive matches in validation/snapshot code, so every later substage fills a variant rather than adding an untyped fallback.

**Gate:** Every backend-generated function and name-based selection is in the matrix with a target family or an explicit negative reason; key tests prove namespace separation among all families, per-instance separation of per-site artifacts (same syntax, two instances → two keys), dedup of structurally equal type-keyed artifacts, and encoding stability.

## Stage 4.2 - Fixed-Point Artifact Closure Engine

> **Separate plan recommended** (`STAGE_4_2_ARTIFACT_CLOSURE_PLAN.md`). This substage changes the shape of the Stage 3 pipeline (worklist and materialization become resumable) and every later substage depends on its API.

- Recommended design: after Stage 3.4 materialization, run a round-based closure loop inside `Lowerer::lower`:
  1. Scan every not-yet-scanned owner (initializers, instances, artifacts) in catalog order and each site in lowered evaluation order; request the artifacts the site needs (drop sites, allocations with finalizers, closure constructions with finalizers, reactive operations, `coro` creations, intrinsic sites, constructor/structural sites already requested by Stage 3).
  2. Expand each newly reserved artifact into its plan. Plan construction may request source-function instances through the Stage 3.2 resolver (`resolve_instance_request` with a `Root` target and an explicit substitution/evidence recipe) and further artifacts.
  3. Feed newly reserved instances back into the Stage 3.3 traversal (made resumable over the existing append-only arenas and catalog) and Stage 3.4 materialization for just those instances.
  4. Repeat until a round reserves nothing. Bound the loop defensively and diagnose non-convergence with the requesting origin chain.
  Rationale: drop/finalizer/pass-mode facts are only concrete after Stage 3.4 recomputation, so scanning materialized bodies avoids re-deriving them during template traversal.
- Refactor `WorklistBuilder` and `materialize_instance_bodies` so they can resume from an existing `SpecializationParts`/program state without re-reserving or renumbering; the Stage 3 validators must still accept the grown graph (dependency edges from artifact-requested instances, new request kinds).
- Artifact-to-artifact and artifact-to-instance edges are recorded on the artifact; site-to-artifact edges are recorded on the owning instance/initializer body binding tables (extend `LoweredBoundTarget::Artifact` use to the new sites).
- Resolve every existing `LoweredCompilerHelperRequest` to an instance or artifact, then remove the unresolved representation (or make any remaining entry a validation error).

**Gate:** A synthetic test family (stub plans) proves: convergence with artifact→instance→artifact chains, recursion through artifacts (drop glue for a recursive nominal type, structural Debug over a type whose Debug impl is generic), no renumbering of Stage 3 ordinals, deterministic ordinals across repeated runs and differing `HashMap` orders, and unchanged Stage 3 results for programs with no generated artifacts.

## Stage 4.3 - Structural Trait Methods, Constructor Adapters, and Formatting

> **Separate plan recommended** (`STAGE_4_3_STRUCTURAL_AND_FORMATTING_PLAN.md`) covering the seven `StructuralTraitMethod` kinds individually.

- Constructor adapters: plan records constructor `SymbolId`/`TypeId`, recursive-construction class (`Ref` vs ordinary), concrete callable type, and the parameter-to-representation mapping the backend uses.
- Structural methods, each with an owned plan built from `StructuralMethodKey` and owned type/trait catalogs (never `TypedModule`):
  - `Debug` (product and sum): ordered labels/punctuation literals, per-field or per-alternative value types, and the selected `Debug` method per element as an instance or nested structural artifact (resolved through `TraitSelectionContext`/`resolve_trait_evidence`, with the debug trait from `LoweredSemanticIds`), plus the `Formatter.write` instance from `LoweredStringFormatting`.
  - `Index`/`MutateIndex`: product element order, per-index result coercions, top-level product flattening the backend observes.
  - `DerefIndex`/`DerefMutateIndex`: the delegated `Index`/`MutateIndex` selection for the dereferenced type.
  - `IntoIterator`/`Iterator` (`next`): iterator product shape, cursor type, item coercion, and the `IterStep` `Done`/`Yield` alternative indices resolved from owned type metadata instead of representation matching during emission.
- Formatting: record the `Formatter.write` edge for template literals and generated Debug punctuation; verify interpolation Display/Debug sites already bind to instances or structural artifacts after Stage 3.4.

**Gate:** Every structural kind has fixtures (nested products, sums, `Ref` targets, generic element Debug impls, iterators over mixed products) whose plans name every callee; plans agree with the legacy backend's choices via `legacy_emissions` (Stage 4.7 hook can be introduced here early); no plan construction calls `TypedModule`.

## Stage 4.4 - Ownership Cleanup: Drop Glue, Finalizers, and Clone

> **Separate plan recommended** (`STAGE_4_4_CLEANUP_ARTIFACTS_PLAN.md`). Cleanup is recursive over types, touches every body family's drop facts, and is the most ABI-sensitive part of Stage 4.

- Define `DropGlue(CanonicalType)` with a plan mirroring `compile_drop_value`/`compile_conditional_drop` exactly: selected user `Drop` method (owned re-implementation of `drop_method_for` over the trait-implementation catalog, including its exact-argument matching rule, and resolving to a `FunctionInstanceId`), then representation drop for `Distinct`; coroutine cleanup; scheduler/wait/resolver/completion-token runtime release; CString free; product elements and sum alternatives that need drop (via `concrete_needs_drop`), in the backend's order. Decide and document whether drop glue is emitted as a callable artifact or kept as an inline plan referenced by drop sites (recommendation: a keyed plan the backend may still inline, so Stage 5 does not change code shape or ABI).
- Bind every drop fact on lowered bodies (discarded values, replaced assignments, loop-body results, `drops_after_call`, temporaries, captures, owned cells, scope exits, early return/propagation/cancellation cleanup) to its `DropGlue` artifact.
- Finalizers: `Payload` (managed `Ref` allocation), `Cell` (captured binding cells), `ClosureEnvironment` (keyed by the closure instance/artifact and ordered concrete capture types, replacing `active_type_substitutions`), and `Buffer` (element type); each references its element drop glue. Bind every allocation/closure-construction/buffer-creation site that sets a finalizer, including coroutine thunk environments.
- `BufferClone` intrinsic sites: record the element `Clone` evidence (resolved through owned catalogs with the new `clone_trait` semantic ID) and the destination buffer finalizer on the intrinsic call record.

**Gate:** Fixtures for user `Drop` impls, nested products/sums with droppable fields, recursive nominal types, closures capturing droppable values in generic instances, captured cells, `Ref` payloads, buffers of droppable and cloneable elements, and coroutine-valued fields. A transition test agrees every drop-glue plan with `TypedModule::drop_method_for`/`type_needs_drop` and every finalizer key with the legacy finalizer set (Debug-string keys mapped through canonical types).

## Stage 4.5 - Coroutine Resume/Cleanup and Reactive Runners

> **Separate plan recommended for coroutines** (`STAGE_4_5_COROUTINE_ARTIFACTS_PLAN.md`); reactive runners are small enough to include in it or inline here.

- Coroutines: key the resume/cleanup pair by the coroutine body thunk's `FunctionInstanceId` (so each instantiation of a generic enclosing function gets its own pair). The plan records frame layout inputs from the owned `LoweredCoroutinePlan` and the instance body (header, resource bundle order, capture order and concrete types, frame bindings, resume-state count, awaited result types, wait/`until` cancellation states), the closure finalizer for the thunk environment, and the drop glue for frame bindings and captures that `cleanup` releases. Bind every `coro` creation site and `await` child link to the pair. Replace `typed_module.coroutine_plan`/`implicit_thunk_for` lookups with plan/instance references.
- Reactive runners: `ReactionRunner`, `UntilRunner`, and `DerivedRunner` keyed by owner + lowered reactive-operation/binding site; plans record the callback's bound instance or indirect closure route, callback closure type, ordered callback resources, and payload slot order. Bind each Stage 2.6 reactive operation to its runner.

**Gate:** Generic functions containing `coro`, `reaction`, `until`, and `derived` instantiated at two types produce distinct artifacts (a regression test that the legacy syntax-keyed names cannot express); nested coroutines, child awaits, cancellation, and task/scheduler fixtures bind every site; plans agree with `coroutine_lower::CoroutinePlan` and the legacy runner set.

## Stage 4.6 - Extern Adapters, Runtime Helpers, and Layout Requirements

- `ExternAdapter` artifacts for every non-variadic extern used as a closure value, keyed by `SymbolId` and canonical callable type, bound to the callable-value sites that use them (the legacy path creates one per extern eagerly; record which are reachable and keep eager-declaration parity in the plan's notes for Stage 5).
- `LoweredRuntimeRequirements`: an ordered, deduplicated set of runtime surfaces (GC, coroutine, reactive runtime modules; UTF-8 validator; libc and LLVM intrinsics) recorded from the lowered operations that need them (string ops, `c_string`, string-literal pattern comparison, numeric-to-string, traps, allocations). The backend may keep installing these by name in Stage 5, but only when the requirement is present.
- Any remaining layout-specific helper found in 4.1 that does not fit an earlier substage is assigned a family here.

**Gate:** Every lazily declared or installed runtime symbol in legacy output is covered by a recorded requirement for fixtures that exercise it, and programs that do not use a subsystem record no requirement for it.

## Stage 4.7 - Closed-Catalog Validation, Transition Comparison, and Handoff

- Validator (new `lower/artifact_validation.rs` or an extension of `graph_validation.rs`), run in `Lowerer::lower` after the closure loop:
  - every callable reference in every instance body, initializer, and artifact plan resolves to an existing instance or artifact of the right family and kind; no `CompilerHelper` or unresolved helper request remains;
  - every artifact key rebuilds from its plan's concrete inputs; keys are unique, planned names are unique and non-empty, and every artifact has at least one recorded requester (no orphans);
  - artifact plans contain no declared parameters, effect variables, `Inferred`, or `Error`;
  - every drop fact, finalizer-setting site, reactive operation, `coro`/`await`, `BufferClone`, and string template is bound to its artifact(s);
  - the catalog is a fixed point: re-running one closure round reserves nothing.
- Transition comparison: extend the `#[cfg(test)]` `legacy_emissions` hook in `codegen.rs` to record every generated function (finalizers, runners, coroutine pairs, structural methods, constructor/extern adapters) and every `trait_method_code`/`ensure_function_specialization` call made from inside generated bodies, then prove each has a matching instance or artifact. Differences must be explained (for example the per-instance split of syntax-keyed artifacts) in the test.
- Add repeated-lowering snapshots of the artifact catalog and plans; add corruption tests for each validator diagnostic.
- Run `cargo fmt --all -- --check`, `cargo check --workspace`, `cargo test --workspace --quiet`, `git diff --check`, and the CLI `--emit llvm`, `--emit object`, and `run` paths with the worktree standard library.
- Update [TYPED_LOWERING_PLAN.md](TYPED_LOWERING_PLAN.md) and this file after each completed substage, recording what passed and what remains, and record the Stage 5 handoff: the exact artifact families, plan fields, and binding tables the backend must consume, and the legacy caches each one replaces.

**Gate:** The catalog is closed and validated for the full standard library and all existing fixtures; the legacy comparison accounts for every generated function; the full workspace suite passes unchanged.

## Ordering and Parallelism

- 4.1 first, then 4.2 (everything else plugs into its API).
- 4.3, 4.4, and 4.5 are independent after 4.2 and may proceed in parallel, with one caveat: 4.5's coroutine cleanup and closure finalizers reference 4.4's `DropGlue`/`GcFinalizer` keys, so land 4.4's key and plan types before 4.5 binds cleanup.
- 4.6 is independent after 4.1 and small.
- 4.7 last; its legacy-recording hook may be added early (during 4.3) and extended per substage.

## Stage Boundary and Definition of Done

- Stage 4 delivers a closed, deterministic, validated catalog of instances and generated artifacts with owned plans; the backend still emits through the legacy path.
- Stage 4 does not make LLVM consume the catalog, remove legacy caches, `active_type_substitutions`, `expression_type_overrides`, or the `TypedModule` bridge; Stage 5 does.
- No ABI, symbol-visible behavior, or source-language change. Target-specific layout stays in the backend.
- Mark Stage 4 complete in the main plan only when every Stage 4.7 gate passes; then identify Stage 5 as next.
