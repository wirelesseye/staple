# Stage 5 Breakdown: Migrate LLVM Generation to Lowered IR

## Status and Goal

**Status:** Stage 5.3 is in progress. Stage 4 finished at `ebe99f3`; the inventory below was frozen against that commit and re-verified against the current tree. At that commit the lowered catalog is closed and validated: every function the backend emits is a `FunctionInstanceId` or an artifact ordinal with an owned plan, every site is bound, and the per-program `LoweredRuntimeRequirements` set is recorded. The legacy backend still ignores all of it: it reads `LoweredModule::typed()` (152 `self.typed_module` reads over about 60 distinct `TypedModule` queries), walks the source AST, and finds specializations and generated functions during emission. Stage 5.2 split its backend-local layer into `codegen/{layout,abi,runtime,ir}.rs` (now `staple-compiler/src/codegen/`, with `mod.rs` holding the legacy emitter) and moved `compile_type`'s typed-module predicates onto a lowered `LayoutContext`; the legacy emitter builds that context from its `LoweredModule`, so layout decisions have one source. Stage 5.3 (lowered emitter skeleton) is underway.

Stage 5 makes LLVM generation consume only `LoweredProgram`. When Stage 5 is done:

- every function is predeclared from the catalog and named by its catalog planned name;
- bodies are emitted by traversing lowered nodes (instance-local arenas for instances, program arenas for initializers), never `staple_syntax::Expression`/`Item`/`Pattern`;
- no `TypedModule`, `ResolvedModule`, `SyntaxId`-keyed cache, `Debug`-string key, `DefaultHasher` name, `active_type_substitutions`, `expression_type_overrides`, `specialization_queue`, `infer_type_parameters`/`substitute_type` call, or LLVM-time trait selection remains in the backend;
- `LoweredModule` no longer owns a `TypedModule`, and the `typed()` bridge is deleted.

Stage 5 keeps target-specific LLVM type layout, calling-convention construction, and instruction emission in the backend. The migration itself (5.1–5.10) changes no language behavior and no concrete closure, resource, coroutine, FFI, or ownership ABI. Its only intended visible difference is the names of internal generated symbols (see Decision D2). After the cutover, Stage 5.11 fixes the two defects the migration deliberately mirrors (Decision D5).

Line references below are against `ebe99f3` and will drift. Re-locate them by function name.

## Required Reading

Agents implementing any substage must read these first:

- [TYPED_LOWERING_PLAN.md](TYPED_LOWERING_PLAN.md): the Stage 5/6 contract and the assumptions.
- The **Stage 5 handoff** section of [STAGE_4_GENERATED_ARTIFACTS_BREAKDOWN.md](STAGE_4_GENERATED_ARTIFACTS_BREAKDOWN.md). It lists each catalog family, the plan fields Stage 5 reads, the binding tables and use sites that reference it, the legacy state it replaces, and the gaps Stage 5 inherits.
- The per-family schemas in the Stage 4.3–4.5 plans ([4.3](STAGE_4_3_STRUCTURAL_AND_FORMATTING_PLAN.md), [4.4](STAGE_4_4_CLEANUP_ARTIFACTS_PLAN.md), [4.5](STAGE_4_5_COROUTINE_AND_REACTIVE_ARTIFACTS_PLAN.md)). They record exactly which legacy decision each plan field mirrors.
- The Stage 4.1 negative matrix, which lists what stays backend-local.

## Migration Contract

1. **Lowered IR is the only input.** The new emitter receives `&LoweredProgram` (plus target data). If it needs a fact that lowering does not record, lowering records it first (Stage 5.1 or the owning substage) and validation covers it. The backend never re-derives a fact with the checker's helpers, never calls the Stage 3.2 resolver, and never selects a trait implementation.
2. **Build a parallel emitter; do not migrate in place.** Legacy expression emission is recursive over the AST: `compile_expression` takes `&Expression`, and every family calls every other. So a family cannot move to lowered IR while its children stay on the AST. Stage 5 builds a new emitter next to the legacy one. Both share the backend-local layout/ABI/runtime layer that Stage 5.2 extracts, and the new emitter replaces the legacy one in a single cutover (Stage 5.10). While Stage 5 is in progress, the new emitter never calls into legacy emission for any construct. A construct it does not yet support fails with a diagnostic, which is a temporary unfinished-family error and not a fallback. "No mixed AST/IR fallback" from the main plan therefore holds at every commit.
3. **Behavior parity with legacy is the oracle.** Emitted programs must behave the same (stdout, exit status, traps) and every emitted function must keep its LLVM function type. The Stage 4 transition tests already prove the plans agree with legacy decisions. Stage 5 proves the emitted code agrees too.
4. **Deterministic emission.** Declaration and body order are fixed: instances in ordinal order, then artifacts in ordinal order, then module initializers in initialization order, then `main`. Nothing iterates a `HashMap` to decide emission order or cleanup order.
5. **Exhaustive matches over lowered kinds.** Every `LoweredExpressionKind`, `LoweredItemKind`, pattern, place, callable target, intrinsic, `LoweredArtifactPlan` variant, and `ArtifactUseSite` is matched with no `_ =>` arm. That way a new lowered variant cannot compile until the emitter handles it. The temporary "not yet implemented" diagnostics sit inside explicit arms, and Stage 5.9 removes the last of them.
6. **Gates.** Every substage ends with `cargo fmt --all -- --check`, `cargo check --workspace`, `cargo check --workspace --features staple-compiler/lowered-emitter`, `cargo test --workspace --quiet` (default backend), the substage's differential corpus, `git diff --check`, and the CLI `--emit llvm`, `--emit object`, and `run` paths with the worktree standard library. Run tests in quiet mode first and re-run verbosely only when something fails.

## Recorded Decisions

All six decisions (D1–D6) were confirmed before Stage 5 started. Stage 5.1 still records any amendment its inventory forces, with the reason.

- **D1: Parallel emitter behind a selector.** Add a `#[doc(hidden)] pub enum Emitter { Legacy, Lowered }` and a `#[doc(hidden)]` `CodeGenerator` constructor that selects it, so in-process differential tests (in `staple-compiler/tests` and `staple-cli/src/compile.rs`) can run both emitters. Add a `lowered-emitter` cargo feature on `staple-compiler`, forwarded by `staple-cli`, that flips the default so the whole workspace suite can run on the new emitter. Stage 5.10 deletes both the selector and the feature.
- **D2: Symbol naming.** Name artifacts and generic instances by their catalog planned names. An instance of a non-generic template (empty substitutions and evidence) keeps the template's existing mangled name (`__staple_m{path}.{name}`). This preserves the `tests/modules.rs` assertions, readable IR, and any external linkage expectations. The choice belongs to the catalog: extend `SpecializationCatalog::planned_names` (Stage 5.1) so it returns the declared name for those instances and still collision-checks every name. The backend reads names and never builds them. Module initializers keep `__staple_init_m{prefix}`, and extern symbols and runtime symbols keep their fixed names. IR assertions that name legacy-only symbols (`__staple_structural_Debug…`, `__staple_gc_finalize_closure_…`, `__staple_coro_…`, `__staple_constructor_…_{hash}`, `__staple_*_runner_{SyntaxId}`) move to planned-name prefixes in Stage 5.9.
  - **Amendment (Stage 5.1, from the inventory):** the declared name is `LoweredFunction::name` verbatim. The resolver has already mangled it: `__staple_m{prefix}.{name}` for the standard library and multi-module programs, and the bare name for single-module code and companion functions such as `to_string`. Some declared names are also already taken when legacy declares functions. The runtime modules are installed first (`reactive.ll` declares libc `write`, so the standard library's `Formatter` `write` method becomes `write.1`), non-intrinsic externs are declared next (with the `.arityN` suffix for overloads), and two distinct templates can share a name. Legacy lets LLVM suffix the later definition. The catalog instead gives an instance its declared name only when that name is neither in `LoweredProgram::reserved_symbol_names` (every runtime-module symbol, every non-intrinsic extern name, and the fixed backend helpers `main`, `__staple_is_valid_utf8`, `free`, `memcmp`, `snprintf`, `strlen`, `memchr`, and `llvm.trap`, which the backend looks up by name) nor already assigned in catalog order. Otherwise it falls back to `__staple_instance_{ordinal}`. The reserved set does not depend on `LoweredRuntimeRequirements`, so names are stable whichever runtime modules a program installs. `LoweredProgram::declared_name_resolver` computes the set once per naming pass. Module globals and initializers are declared after functions, so they never displace a function name; 5.3 must name them so that they avoid the planned names.
- **D3: Drop glue is expanded inline at each use site** from its `DropGlueBody`, recursing through nested `PlannedArtifact` glue. This matches legacy code shape (the Stage 4.7 census already explains glue as "inlined at its sites"). No `__staple_drop_glue_N` function is emitted. Finalizer, coroutine, runner, adapter, and structural artifacts are emitted as functions exactly where legacy emits functions.
- **D4: Initializer dispatch sites get a binding table in lowering.** Stage 5 does not re-resolve initializer calls with the Stage 3.3 recipe in the backend, because that would be LLVM-time resolution. Stage 5.1 adds an initializer-level `bindings`/`evidence` table, built by the same machinery the 4.4/4.5 scanners use (`initializer_body_instance`/closure resolution) and validated like the instance table.
- **D5: Mirrored legacy defects.** Stages 5.1–5.10 preserve behavior, because parity with legacy is the migration's oracle. They **keep mirroring** the completed-coroutine frame-binding leak (4.5) and the "generic `Drop` implementations are never selected" rule (4.4). Stage 5.11 fixes both right after the cutover, when each fix lands in one backend. Do not fix either earlier: that would mean changing legacy, lowering, and the transition tests, or would put an expected difference into the 5.6/5.8 cleanup comparisons. The syntax-keyed coroutine/`until` aliasing is **fixed by construction**: pairs and runners are emitted per catalog entry. Legacy's `HashMap`-ordered unwind drops become **plan order**. The differential corpus must include the generic-`coro` and generic-`until` fixtures that legacy aliases or fails on, and those are recorded as explained differences.
- **D6: The `main` harness is regenerated from lowered entry and initializer metadata** (initialization order, entry IO/reactive resources, global root regions). The harness always installs the garbage collector for an executable, regardless of `LoweredRuntimeRequirements` (Stage 4.6 hand-off). Every other runtime module and every lazily declared libc/LLVM symbol is installed only when its requirement is present.

## Frozen TypedModule Query Replacement Matrix

These are the distinct `TypedModule`/`ResolvedModule` queries the legacy backend makes at `ebe99f3`, with the lowered record that replaces each one. **Stage 5.1 froze this table**: every row was re-verified against `codegen.rs` at the current tree, and every row without a lowered source became a lowering-side prerequisite (the gap rows below, now closed). "Instance" means `LoweredInstanceBody`, and "symbol" means the `LoweredSymbol` catalog entry.

| Legacy query (count) | Replacement | Verified |
| --- | --- | --- |
| `functions`, `implicit_thunks`, `type_of_function`, `function_by_id`, `function_for` | instance catalog order plus the instance `signature`, and initializer records; the backend never enumerates templates | no gap |
| `type_of_expression`, `coercion_for`, `moved_symbols` | the lowered expression header (checked type, coercion, moved symbols), already concrete in instance bodies | no gap |
| `type_of_symbol`, `symbol_for`, `type_for_pattern`, `type_of_pattern` | pattern bound symbols and pattern types, `LoweredName`, `LoweredInstanceBody::binding_symbol_type`, and parameter/capture records; `LoweredSymbol::name` carries the declared module-level name | no gap |
| `has_mutable_storage`, `is_derived_symbol`, `is_borrowed_capture`, `is_non_owning_symbol`, `is_mutated_parameter`, `is_signal_symbol`, `requires_initialization_state`/`_check` | symbol storage and flags (`mutable_storage`, `captured`, `non_owning`, storage class, initialization facts), instance capture flags, and name/binding initialization facts. Replace `captured_cell_symbols` (computed in `ModuleEmitter::new`) with the Stage 4.6 "actually captured" fact. `LoweredSymbol::module_symbol` supplies the signal global-versus-cell fact (`storage == Signal` plus `module_symbol`) | **gap closed** |
| `type_needs_drop`, `is_copy_in_function`, `drop_method_for`, `is_drop_method` | recomputed drop/pass facts on bodies (Stage 3.4), `DropGlue` plans at bound use sites, `owned_bindings`, and call pass modes | no gap |
| `is_{io,reactive,coroutine,task,scheduler,tasks,wait,resolver,completion_token}_type`, `wait_result`, `task_result` | `runtime_opaque_kind` plus `LoweredSemanticIds` (layout); await records carry result types; `DropGlueBody::RuntimeRelease` covers cleanup | no gap |
| `trait_impl_method`, `structural_trait_method`, `standard_trait`, `trait_for_method`, `trait_dispatch_for`, `instantiated_trait_method_type`, `complete_trait_arguments`, `traits` | the binding table plus `evidence` per site; structural and delegate callees come from plans (`callee_type`). D4 adds the same table for module initializers | **gap closed (D4)** |
| `product_default_plan`, `juxtaposed_call_plan`, `curried_default_plan`, `access_for`, `match_for`, `logical_for`, `propagation_for`, `primitive_macro_for` | `LoweredProduct` steps/layout, call steps and argument slots, `LoweredAccess`, `LoweredMatch`, `LoweredLogical`, item propagation facts, normalized `c_string` payloads | no gap |
| `intrinsic_function`, `symbol_is_overloaded`, `builtin_type`, `constructor_type`, `recursive_construction`, `singleton_type` | the `Intrinsic`/`Constructor` callable targets with recursive-construction class, the type catalog, and singleton name facts. `symbol_is_overloaded` is **naming only** (it does not change the intrinsic or constructor identity); `LoweredSymbol::overloaded` records it | **gap closed** |
| `implicit_thunk_for`, `derived_evaluator`, `coroutine_plan`, `coroutine_parts` | `CallArgumentThunk`, `DerivedEvaluator`, `ReactiveCallback`, and `Coro` bindings; `CoroutineCodesPlan` and the body instance's `plans[0]` | no gap |
| `resource_for_expression`, `io_resource`, `reactive_resource`, `entry_reactive_required` | `LoweredResourceUse`/provider records, call `resource_bindings`, entry resource metadata, and `LoweredSemanticIds`. Entry metadata is `LoweredInitializer::resources` plus the `EntryParameter` providers, in installation order | no gap (verified closed) |
| `string_representation`, `resolved().program`, `mangled_module_prefix` | `LoweredSemanticIds` string representation, the module catalog plus initialization order, and `LoweredModuleInfo::symbol_prefix` | **gap closed** |
| `standard_function_named`/`standard_function_name_matches` (formatting) | the `FormattingConstructor`/`Write`/`Finish` bindings. The name matcher disappears from the backend | no gap |
| `declare_top_level_storage` AST walk (`Item::Binding`/`Item::PatternBinding`, `binding.name`, `binding.type_parameters`, `self.globals`) | `LoweredSymbol::{name, has_global, overloaded}` and `LoweredModuleInfo::symbol_prefix`: the emitter declares globals from symbol records, never syntax | **gap closed** |
| `compile_main_function` global scan (`self.storage` x `checked_type_contains_ref`) | `LoweredSymbol::{has_global, global_root}`; the root-region size stays backend layout | **gap closed** |

## Legacy State Removal Matrix

Each item is removed by the substage whose family stops needing it, and is gone from the tree after Stage 5.10. The new emitter never introduces any of them.

| Legacy state | Replaced by | Removed from new emitter by |
| --- | --- | --- |
| `functions`, `specialized_functions`, `specialization_queue`, `compile_queued_specializations`, `ensure_function_specialization`, `specialization_key` | catalog predeclaration indexed by `FunctionInstanceId` | 5.3 |
| `active_type_substitutions`, `expression_type_overrides`, `concrete_expression_type`, `infer_type_parameters`/`substitute_type`/`contains_type_parameter` imports | concrete instance bodies | 5.3–5.5 |
| `constructor_codes`, `ensure_constructor_adapter` | `ConstructorAdapter` artifacts | 5.4 |
| `closure_codes` (extern entries), `declare_external_functions` adapter bodies | `ExternAdapter` artifacts, plus a plain foreign-symbol declaration for every extern | 5.4 |
| `structural_trait_codes`, `trait_method_code`, `standard_trait_id`, `trait_method_id`, `build_trait_method_call`, `standard_function_named` | bindings and structural plans | 5.7 |
| `gc_finalizers` (`Debug`/hash keys), `ensure_*_finalizer`, inline `compile_drop_value` type recursion | `GcFinalizer` artifacts and inline `DropGlueBody` expansion | 5.6 |
| `coroutine_codes` (body `SyntaxId`), `ensure_coroutine_codes` discovery, `is_coroutine_body_thunk` | `CoroutineCodes` artifacts | 5.8 |
| `__staple_*_runner_{SyntaxId}` names, `emit_until_runner` name reuse | runner artifacts | 5.8 |
| unconditional `install_*_runtime`, lazy `get_function(..).unwrap_or_else(add_function)` | `LoweredRuntimeRequirements`, plus the D6 harness rule | 5.3 |
| `LoweredModule::typed`, `Box<TypedModule>` payload, the `#[cfg(test)]` legacy recorder and `legacy_emissions` | nothing | 5.10 |

## Stage 5.1 - Inventory, Decisions, and Lowering-Side Prerequisites

Everything in this substage lives in lowering or is documentation. It adds no emitter code.

- Freeze the query replacement matrix above. Walk every `typed_module` and `resolved()` read, every AST type the backend matches on (`Expression`, `Item`, `Pattern`, `CallExpression`, `ProductExpression`, `RepeatedProductExpression`, `PatternBindingKind`), and every helper in `codegen.rs` that takes a `SyntaxId`. For each, record the lowered field that supplies it, or open a prerequisite. Record the frozen table here, as Stage 4.1 did for routes.
- Confirm or amend decisions D1–D6.
- **Initializer binding table (D4).** Add `bindings`/`evidence` storage for module initializers, keyed by `LoweredBindingSite` over program-arena IDs. Build it at the closure fixed point with the existing initializer resolution recipe. Validate it with the instance-table rules (every dispatch site bound; bindings agree with request roots and closure edges). Add corruption tests.
- **Planned names (D2).** Extend `planned_names` to take the non-generic-template name, keep collision checking across all families, and add determinism and collision tests. Expose `planned_name(FunctionInstanceId)`/`planned_name(ArtifactOrdinal)` accessors.
- **Backend read view.** The emitter cannot reach the `pub(super)` arenas. Add a read-only `pub(crate)` view (for example `lower::emission` with `LoweredProgram::emission_view()`), and promote `OwnerArenas` (currently `pub(super)` in `lower/cleanup_artifacts.rs`) to a shared owner view: one type that resolves a block/item/expression/pattern/place/call/callable-value/provider/use/with/reactive/plan/coro/await ID against either an instance body or the program arenas of an initializer. Add `LoweredModule::program()`. Expose read-only accessors only; do not make the arenas public.
- Close every other gap the matrix finds (expected candidates: `symbol_is_overloaded`, signal global versus cell storage metadata, global-root region facts for the harness, entry resource metadata). Each gap gets a lowering record, a validator arm, and a snapshot line.

**What landed:**

- **Frozen matrix.** Every row was re-verified against `codegen.rs`; the table above records the replacement and whether a gap existed. The `declare_top_level_storage` walk and the `compile_main_function` global scan were added as explicit rows. Entry resource metadata needed no new record: `LoweredInitializer::resources` plus the `EntryParameter` providers already carry installation order and provider facts. D1 and D3–D6 are confirmed unchanged; D2 carries the fallback amendment recorded above.
- **D4 initializer tables (new `src/lower/initializer_bindings.rs`).** `LoweredProgram` gained `initializer_bindings`/`initializer_evidence`, indexed by `InitializerId`, keyed by `LoweredBindingSite` over program-arena IDs. `bind_initializer_sites` runs at the closure fixed point (after `record_runtime_requirements`, before names are assigned) and walks each initializer with the shared `LoweredWalker`. It resolves each site with the same Stage 3.3 recipe the scanners use (root target, no enclosing environment). The sites covered are direct, trait, and structural calls; callable values; constructor adapters; implicit thunk arguments; formatting helpers and interpolations; index reads; indexed assignment; derived evaluators; reaction/`until`/`batch` callback thunks; `coro` body thunks; and await child links. `validate_initializer_bindings` re-walks each initializer and checks:
  - every dispatch site is bound with the expected shape;
  - table keys are live and targets exist;
  - trait sites have evidence;
  - closure-phase uses name the bound instance;
  - every instance first requested by the initializer is bound somewhere.

  It then **re-resolves every site from scratch and requires both tables to equal the fresh ones**. A site bound to another existing instance passes every structural check, so only the re-resolution catches it. `LoweredBoundTarget` and `TraitEvidence` gained `PartialEq` for this. Seven corruption tests cover the rules, including two sites with swapped bindings.
- **One walker extension (D4 support).** The shared owner walker gained `call_id_site`/`item_site`/`expression_site`/`await_id_site` default hooks so a visitor can key bindings by owner-local ID, and a `walks_derived_binding_values` switch: cleanup scanning stops at a derived binding, but the initializer binder must walk its value because the worklist requests those sites under the enclosing owner (a derived module binding's `count + count` is one such request root). The binder and its validator both enable it.
- **Planned names (D2).** `SpecializationCatalog::planned_names_with(declared)` computes names in one pass, applying the declared-name rule and the fallback; `planned_names()` keeps the no-resolver form. `LoweredProgram::declared_name_resolver` supplies the verbatim declared name, filtered by the reserved set. `planned_name`/`planned_artifact_name` return the name the worklist assigned to the instance or artifact record (a borrowed `&str`, with no recomputation). `validate_specializations` now checks artifact record names against the planned vector, as it already did for instance names. `GraphRecorder::assign_names` and every validator use the same resolver, so assigned names and the planned vector cannot drift. Unit tests cover the declared rule, the ordinal rule, artifact names, and both collision shapes. `stage_5_1_planned_names_match_legacy_declared_names` compares every eagerly emitted non-generic instance against the LLVM name the legacy backend actually defines. It requires an exact match for a free name, the ordinal fallback where legacy was renamed (the `write` clash is asserted), no double prefix, and ordinal names for generic instances.
- **Backend read view (new `src/lower/emission.rs`).** `EmissionView` exposes read-only access to:
  - catalog iteration and planned names;
  - symbol, function, type, and module metadata;
  - semantic IDs, string formatting, and runtime requirements;
  - per-owner binding/evidence tables;
  - instance captures, parameters, and signatures;
  - owner-uniform `artifact_uses`/`instance_uses`/`owned_bindings`, which read the initializer storage or the instance body the same way, so the emitter reaches drop glue, finalizers, pairs, runners, and scope-exit records for both owner shapes;
  - the owner resolver.

  Nothing in the view is mutable, and no arena is exposed. `OwnerArenas` moved here, is now `pub(crate)`, and gained provider, use, and coroutine-plan resolution alongside its existing accessors. `LoweredProgram::emission_view()` and `LoweredModule::program()` construct the view. Tests prove both owner shapes resolve, both binding tables are readable, and the use and owned-binding records are readable for an initializer `coro` creation and an instance block local.
- **Gap closure records.** `LoweredSymbol` gained `name` (the declared module-level binding/extern name), `overloaded` (`symbol_is_overloaded`, which the inventory confirmed is naming only), `module_symbol` (signal global versus cell), `has_global`, and `global_root` (the harness root-region fact ported from `checked_type_contains_ref`). `LoweredModuleInfo` gained `symbol_prefix`. `SymbolDeclarationFacts` collects the names and global-storage set in one AST pass at symbol snapshot time, mirroring the legacy storage conditions. `validate_symbols` re-checks the storage/scope/name/root invariants, and the catalog snapshot prints every symbol's new facts. A test covers a `Ref` global root, a plain global, and two arity overloads.
- **Snapshot and determinism.** The Stage 4.7 `catalog_snapshot` now includes initializer bindings/evidence and the symbol facts; `repeated_lowering_yields_identical_catalogs_and_plans` re-checks it across three from-scratch lowerings.

**Gate:** The frozen matrix has no row without a lowered source. The new tables and accessors are validated and snapshotted, and repeated lowering stays byte-identical. The full suite passes unchanged, since the legacy backend is untouched. `cargo fmt --all -- --check`, `cargo check --workspace`, `cargo test --workspace --quiet` (1283 tests), `git diff --check`, and the CLI `--emit llvm`, `--emit object`, and `run` paths with the worktree standard library pass. (The `staple-compiler/lowered-emitter` feature in Contract 6 does not exist yet; D1 introduces it in Stage 5.3, so that check starts there.)

## Stage 5.2 - Extract the Backend-Local Layer

This is a behavior-preserving refactor of `codegen.rs` into a `codegen/` module tree, so both emitters share one layout/ABI/runtime layer and the legacy emitter keeps working unchanged.

- Split out, with no `TypedModule` parameter:
  - `layout.rs`: `compile_type` and the integer, float, product, sum, closure, and slice types; `SumStorage`; `buffer_header_type`; `coroutine_header_type`, `task_record_type`, `completion_record_type`, `coroutine_resource_bundle_type`, and `until_frame_type`; the `CORO_*`/`COMPLETION_*`/`TASK_RECORD_*` constants.
  - `abi.rs`: `compile_native_function_type`, `compile_closure_function_type`, `compile_parameter_types`, `indirect_parameter_mask`, `mutation_parameter_mask`, `flattened_parameter_types`, and `build_fn_type`.
  - `runtime.rs`: the `gc.ll`/`reactive.ll`/`coroutine.ll` installs, `build_utf8_validator`, `coroutine_runtime_fn`, `build_reactive_runtime_call`, and the lazy libc declarations.
  - `ir.rs`: pure IR helpers (`build_trap_if`, byte helpers, `build_gc_allocation`, `set_gc_finalizer`, `register_gc_root_region`, `register_gc_interior`, `unit_value`, `value_as_basic`).
- Replace the typed-module predicates inside `compile_type` (`is_io_type` … `is_completion_token_type`, currently near line 13166) with a `LayoutContext` built from `LoweredSemanticIds`, the type catalog, and `runtime_opaque_kind`. The legacy emitter builds the same context from its `LoweredModule`, so layout decisions have one source from now on.
- Move the `#[cfg(test)]` legacy recorder types to `codegen/legacy_recorder.rs` unchanged.

**What landed:**

- **Module tree.** `staple-compiler/src/codegen.rs` became `codegen/mod.rs` (the legacy `CodeGenerator`/`ModuleEmitter`), with `codegen/layout.rs`, `codegen/abi.rs`, `codegen/runtime.rs`, `codegen/ir.rs`, and `codegen/legacy_recorder.rs`. `include_str!` paths for `gc.ll`/`coroutine.ll`/`reactive.ll` moved with the installs.
- **Shared backend layer.** `Backend<'program, 'context>` owns the LLVM context/module/builder, target data, pointer-sized integer type, and `LayoutContext`. It is `pub(crate)` with a private constructor (Stage 5.3's emitter is a child module). `ModuleEmitter` embeds it and reaches its fields and methods through `Deref`/`DerefMut`, so the legacy emitter's 900-odd `self.context`/`self.builder`/`self.llvm_module`/`self.target_data`/`self.size_type` uses are unchanged. The mutating IR helpers (`build_gc_allocation`, `set_gc_finalizer`, `register_gc_interior`, `build_trap_if`) take `&self` because inkwell's builder/module API is interior-mutable; that also avoids `deref_mut` two-phase-borrow conflicts at call sites that pass `self.size_type`/`self.target_data` expressions.
- **`layout.rs`** holds the record field-index constants (`CORO_*`, `COMPLETION_*`, `TASK_RECORD_*`), `SumStorage`, `compile_type` with the integer/float/product/sum/closure/slice helpers, `buffer_header_type`, `coroutine_header_type`, `task_record_type`, `task_record_header_type`, `completion_record_type`, `coroutine_resource_bundle_type`, and `until_frame_type`.
- **`abi.rs`** holds `compile_native_function_type`, `compile_closure_function_type`, `compile_parameter_types`, `indirect_parameter_mask` (the always-`None` `function` parameter is gone), `build_fn_type`, and the free `mutation_parameter_mask`/`flattened_parameter_types`.
- **`runtime.rs`** holds the three `.ll` installs, `build_reactive_runtime_call`, `build_utf8_validator` (it returns the function, and the emitter wrapper records the test-only origin), and `declare_named_function`. That single helper declares any symbol the emitted code calls by name: the coroutine runtime helpers (formerly `coroutine_runtime_fn`) and the five libc functions (`free`, `memcmp`, `snprintf`, `strlen`, `memchr`) that previously used inline `get_function(..).unwrap_or_else(add_function)` calls.
- **`ir.rs`** holds `build_trap_if`, the byte helpers (`increment_utf8_index`, `byte_in_range`, `byte_equals`), `build_gc_allocation`, `set_gc_finalizer` (the emitter wrapper still sets the test-only `legacy_finalizer_set` flag), `register_gc_root_region`, `register_gc_interior`, `unit_value`, and `value_as_basic`.
- **`LayoutContext`.** Built from `EmissionView` (new `concrete_is_copy` and `runtime_opaque_kind` accessors, plus the re-exported view type in `lower.rs`), it answers `string_representation`, `is_copy`, `is_io`, `is_reactive`, and `is_pointer_runtime` from the lowered semantic IDs, so `compile_type`, the closure ABI, and the indirect-parameter mask no longer read `TypedModule` predicates. Every opaque check goes through `opaque_is`, where an absent semantic ID never matches. A program without the standard library's `IO` type must not classify every non-opaque type as `IO` through `None == None`; `an_absent_semantic_id_matches_no_type` covers this. `TypedModule::is_io_type` and `is_task_type` are now read only by the agreement test and are `#[cfg(test)]`.
- **Recorder.** The `#[cfg(test)]` record types moved to `legacy_recorder.rs` unchanged; the record *state* and the `legacy_emissions`/`referenced_runtime_symbols` constructors stay with the emitter in `mod.rs`.

**Gate:** `cargo fmt --all -- --check`, `cargo check --workspace`, `cargo test --workspace --quiet` (1283 tests, unchanged; the new layout agreement test brings the count to 1284, and the review's absent-ID test to 1285), `git diff --check`, and the CLI `--emit llvm`, `--emit object`, and `run` paths with the worktree standard library pass. The IR comparison used a 16-program corpus: the five Stage 4.7 census programs plus every working `staple-compiler/examples` program, including the `game_loop` module pair (`macros.sta` still fails during lowering at `main`, unchanged by this refactor). Each program was compiled repeatedly with the `cc0cb06` binary and the Stage 5.2 binary, normalized by sorting definitions by name and sorting `__staple_gc_register_root` calls, and compared function by function. Fifteen programs have exactly one normalized variant on both sides and it is byte-identical. `example_coroutines` has exactly four variants on both sides (two coroutine frames, each with a two-cell `HashMap`-ordered binding-to-index assignment), and the variant *sets* are equal. That legacy nondeterminism exists at `cc0cb06` too (the same binary yields different field indices run to run) and is not introduced here. The new `layout_context_agrees_with_checker_predicates` test locks `LayoutContext`'s `Copy`/`IO`/`Reactive`/pointer-handle decisions to the checker predicates across a five-fixture sweep.

**IR comparison tool.** The comparison is now reproducible with `scripts/compare-llvm-ir.py --old <baseline staple> --new <staple under test> --stdlib stdlib [--runs N] [--show-diff] <programs>`. The script normalizes as described above, compiles each program `--runs` times per binary, and compares the *sets* of variants. It reports `same`, `OVERLAP` (the sets share a variant but differ, which sampling can cause; re-run with more runs), or `DIFF` (disjoint sets), and exits non-zero on anything but `same`. Stages 5.3–5.9 use it for their differential checks. An independent review re-ran the comparison with it against a separately built `cc0cb06` binary on the examples plus an ABI probe (generic `move` functions, a non-`Copy` product, `move CString`, a `mut` parameter) and a coroutine/C-string program: every program matched, and `coroutines` showed the same four variants on both sides over 16 runs each. The review fixes (`opaque_is`, the merged `declare_named_function`, `#[cfg(test)]` on the two accessors) were checked the same way and leave the IR unchanged. The suite then passes 1285 tests.

## Stage 5.3 - Lowered Emitter Skeleton, Declarations, Harness, and Differential Harness

**Implementation progress:** Stage 5.3 is in progress. The D1 `Emitter` selector and `lowered-emitter` features route LLVM and object emission through a parallel `LoweredEmitter`. It owns a read-only `EmissionView` and the shared `Backend`. It installs GC, the required coroutine/reactive modules and named libc/UTF-8 surfaces; predeclares instances, every planned function artifact (coroutine pairs as two functions, drop glue as no function), externs, module globals and metadata, and initializers; dispatches instance/initializer blocks through exhaustive lowered item/expression matches; binds simple parameters and captures; and builds `main` from lowered initializer and GC-root records. Unsupported families return source diagnostics. The standard library's eager bodies still reach those diagnostics, so the runnable corpus, complete parameter/pattern handling, artifact bodies, full census/type comparison, and CLI differential harness remain. The gate below has not passed.

The next declaration pass adds entry IO/reactive resource setup and reactive scope disposal from initializer records. A test-only declaration snapshot runs before unported bodies and compares LLVM function types with the legacy recorder for eager instance declarations, module initializers, and reachable constructor adapters; it also checks the unique declaration count against the instance/artifact/initializer catalog on a standard-library fixture. This is a partial ABI comparison. The remaining artifact families, full defined-function mapping, verified runnable corpus, and CLI behavior comparison are still needed for the gate.

- Add `codegen/lowered/mod.rs` with `LoweredEmitter<'program, 'context>`. It borrows the emission view and the shared layer and has no `TypedModule` field. Add the D1 selector and feature.
- **Predeclaration from the catalog:**
  - every materialized instance, with its concrete signature through `abi.rs` and its planned name (coroutine body-thunk instances are not declared as ordinary functions; their pair artifact is);
  - every artifact that D3 emits as a function;
  - module initializers;
  - a foreign-symbol declaration for every extern;
  - top-level storage, initialization-state, signal, and derived metadata globals, from symbol and initializer records (the replacements for `declare_top_level_storage`, `declare_pattern_storage`, and `declare_initialization_state`).
- **Emission order:** instances in ordinal order, then artifacts, then initializers, then `main` (Contract 4). Install runtime modules and lazy symbols from `LoweredRuntimeRequirements` (D6). Regenerate `main` from lowered metadata (D6).
- **Function environment:** port `FunctionEnvironment` to owner-local IDs. Locals, owned, binding cells, and parameter pointers are keyed by `SymbolId` as today, while anything keyed by expression identity uses the owner-local `ExpressionId`. Port parameter binding (`bind_function_parameters`, `bind_mutable_parameter_pointers`, `bind_environment_captures`) against instance parameter and capture records. Add the owner dispatch loop (walk the root block's items, then its result) with an exhaustive item/expression match whose unported arms return `Diagnostic::new(origin.span, "lowered emitter: <family> is not implemented yet")`.
- **Differential harness.** Put it in a test helper shared by `staple-compiler/tests` and `staple-cli`:
  - compile each corpus program with both emitters and require LLVM verification on both;
  - map every legacy-defined function to its catalog entry through the Stage 4.7 census machinery (`LegacyFunctionOrigin` → instance or artifact → planned name), then require the new module to define exactly the mapped set, minus the census's explained items, with **identical LLVM function types**;
  - in `staple-cli`, compile, link, and run with both emitters and compare stdout and exit status.
  - Keep a per-substage corpus list in the harness. Each later substage appends its fixtures, and the list only grows.

**Gate:** The corpus of an empty `main`, integer arithmetic with non-generic functions, and module initializers with globals passes the differential harness. The catalog declaration count equals the census mapping on the standard library, and all declarations verify. `rg` finds no `typed_module`, `TypedModule`, `staple_syntax::Expression`, or `SyntaxId` in `codegen/lowered/`.

## Stage 5.4 - Functions, Calls, Callable Values, Closures, and Resources

> **Separate plan required:** `STAGE_5_4_CALLS_CLOSURES_RESOURCES_PLAN.md`. This is the backbone every other family depends on. It spans about 2.5k legacy lines (`compile_call_expression`, `compile_indirect_call_value`, `compile_arguments`, `compile_effect_arguments`, `compile_resource_arguments`, `compile_symbol_value`, `build_closure*`, `build_capture_environment`, and the constructor/extern adapters), and the ABI risk concentrates here.

- Port every call category from the binding table: direct instances, selected trait implementations bound to instances, indirect closures, externs (C-string temporaries), and constructor calls (`Value` versus managed `Ref`, with `ManagedRef` finalizer uses). Argument ABI slots, pass modes, mutation pointers, and borrowed/materialized temporaries come from `LoweredCall` records. Hidden resources come from `resource_bindings` in effect-row order. Implicit thunk arguments come from `CallArgumentThunk` bindings. Ordered call steps preserve evaluation order.
- Port callable values and closure construction: `Stored` versus `Fresh` environments, capture order and access, cell captures, and the `ClosureEnvironment` finalizer use (its artifact body is 5.6). Port the `ConstructorAdapter` and `ExternAdapter` artifact bodies here, since they are call shims.
- Port resources: `with` scopes (place-backed versus materialized, scope-exit kinds), `resource` reads, and resource assignment places.
- Port the minimal expression set every runnable program needs: literals, names, blocks, and unit. The remaining expression families are 5.5.
- Leave intrinsic calls behind their family arms. Numeric, string, C-string, bool, and UTF-8 intrinsics land here. Buffer intrinsics are 5.6 (they are ownership-heavy), and coroutine, task, scheduler, completion, and reactive intrinsics are 5.8.

**Gate:** The differential corpus runs `println`-style IO programs, generic direct calls at two instantiations, curried and juxtaposed calls with defaults, closures with value, `mut`-cell, and borrowed captures in generic instances, extern calls and extern closure values, constructor calls and constructor values (including `Ref`), `with`-provided resources, and the numeric/string/C-string intrinsics. LLVM function types match for every mapped function.

## Stage 5.5 - Expressions, Patterns, Places, and Control Flow

> **Separate plan recommended:** `STAGE_5_5_EXPRESSIONS_CONTROL_FLOW_PLAN.md`. It mirrors the Stage 2.4 family list and covers `compile_expression_uncoerced` (about 600 lines), the match/pattern machinery (about 900 lines including string-literal patterns and sum coercion), places and indexed assignment, and products.

- Products (step replay, designated, spread, and default plans), repeated products, access, `satisfies`, coercions (sum widening, slice ref, `coerce_value`), logicals, loops (break/continue and loop contexts), matches and all pattern forms (including string-literal pattern comparison and `at` patterns), propagation, assignment places (all place kinds, `MutateIndex` dispatch through the item binding), index reads through the `Index` binding, and initialization checks.
- Loop and match cleanup hooks (`owned_before`, body-result drops) call the 5.6 scope API through a narrow interface. Until 5.6 lands they may emit no drops, and the corpus for this substage avoids droppable values in those positions.

**Gate:** The corpus covers every Stage 2.4 family, including nested matches over sums and products, string-literal patterns, loops with break values, propagation, indexed assignment, and default/spread products. Behavior and function types match legacy.

## Stage 5.6 - Ownership Cleanup, Finalizers, and Buffers

> **Separate plan required:** `STAGE_5_6_OWNERSHIP_CLEANUP_EMISSION_PLAN.md`. Cleanup is the most ABI- and behavior-sensitive area. It depends on exact ordering (`owned_order`, reverse drops from scope marks, early return, propagation, and cancellation exits), and D3's inline expansion has to reproduce `compile_drop_value`'s code shape.

- Owned-binding registration from `owned_bindings` (value versus cell storage), scope marks, `drop_owned_since`/`drop_all_owned` equivalents, moved-ownership release, and the exit schedule on return, propagation, loop exit, and cancellation. This follows the 4.4 contract.
- Inline `DropGlueBody` expansion (D3) at all sixteen 4.4 use sites: user `Drop` instance calls, coroutine cleanup, runtime releases, C-string free, and product/sum/`Distinct` recursion, with conditional (live-flag and cell-state) variants.
- `GcFinalizer` artifact bodies (all four subkinds), and every finalizer-setting site.
- Buffer intrinsics (`compile_buffer_*`), including `BufferClone` with its `BufferCloneElement` instance use and destination finalizer.
- Wire the 5.5 loop and match hooks to real drops.

**Gate:** The Stage 4.4 fixture set (user `Drop`, nested droppable products and sums, recursive nominal types, generic-instance closures capturing droppables, captured cells, `Ref` payloads, buffers of droppable and cloneable elements, coroutine-valued fields) runs identically. A drop-order probe fixture (a `Drop` impl that prints) produces identical output under both emitters, including early return and propagation exits.

## Stage 5.7 - Structural Methods and Formatting

No separate plan is needed: the 4.3 plans already record every decision verbatim.

- Emit the seven structural kinds from `StructuralBody`: `Debug` literals and delegates in order through `callee_type`, the `Index`/`MutateIndex` switch and load, the `DerefIndex` fast path and delegation, `IntoIterator`/`next` with `Done`/`Yield` indices, and the shared `Formatter.write` instance.
- String templates from the `FormattingConstructor`/`Write`/`Finish` and `Interpolation` bindings.
- Delete the new emitter's temporary trait-dispatch arms. Every trait call is already an instance or artifact binding from 5.4.

**Gate:** The 4.3 transition fixtures (nested products and sums, explicit generic `Debug`, index/mutate/iterate/deref, string templates with Display and Debug interpolations) print identical output.

## Stage 5.8 - Coroutines, Tasks, and Reactive Code

> **Separate plan required:** `STAGE_5_8_COROUTINES_AND_REACTIVE_EMISSION_PLAN.md`. `ensure_coroutine_codes` alone is about 650 lines. With awaits, external awaits, drive, scheduler, completion, reaction, `until`, derived, and batch it covers roughly 4k legacy lines, and the state-machine frame layout and cancellation paths are the riskiest control flow in the backend. The plan may split coroutines and reactive work into two sequential steps, but they share the `until` runner and the callback-environment finalizers, so they belong in one plan.

- `CoroutineCodes` pair bodies from `CoroutineFramePlan` and the body instance's `plans[0]`: frame layout inputs (frame bindings in cell order, the result and pending fields, resource slots with pass modes, captures and `capture_finalizer`), resume-state dispatch, `await` suspension by kind (child, `Task`, `Wait`, `until`), the cancellation unwind with `unwind_drop` in plan order (D5), and the completed-body behavior mirrored (D5). `coro` creation comes from the `CoroCreation` use.
- Task, scheduler, and completion intrinsics; `compile_coroutine_drive`; task-scope tracking.
- Signals (global versus cell storage, read tracking, notify on assignment), derived create and read, `Reaction`/`Until`/`Derived` runner artifact bodies from `ReactiveRunnerBody`, `batch`, `scope`, `snapshot`, reactive-scope disposal, and callback/evaluator environment finalizer uses.

**Gate:** The Stage 4.5 fixture set runs identically: nested coroutines, child awaits, cancellation, `Wait`/`until` states, tasks and schedulers, reaction resources, `until` inside a coroutine, derived bindings in initializers and instances, and the droppable-capture evaluator. The generic `coro`/`reaction`/`until`/`derived` fixtures at two instantiations produce two distinct pairs and runners and run correctly. These are the recorded D5 differences: legacy aliases or fails on them, so the harness asserts new-emitter correctness only.

## Stage 5.9 - Full-Suite Parity and New-Emitter Census

- Run the **entire** workspace suite with `--features staple-compiler/lowered-emitter`. Fix every failure in lowering or the new emitter, never by consulting `TypedModule`.
- Update the IR-text assertions that name legacy-only symbols to planned-name prefixes (D2). These are the `tests/compiler.rs` assertions on `__staple_structural_Debug`, `__staple_gc_finalize_closure_`, `__staple_coro_`, and similar. Where the IR shape legitimately differs because of D5, state the reason in the test.
- Add a permanent **new-emitter census**: every function the new module defines outside the runtime modules maps to exactly one catalog instance or artifact (or to `main`/the UTF-8 validator), and every catalog entry is defined or explained (inlined drop glue, coroutine body thunks inside `resume`, coroutine-body instances). This replaces the Stage 4.7 legacy census.
- Remove every "not implemented yet" diagnostic arm. `rg "not implemented yet" codegen/lowered` is empty.
- Run the differential harness over the full union corpus plus the standard library. Require identical LLVM function types for every mapped function and identical run behavior for every CLI run test.

**Gate:** The full suite passes on both emitters. The census passes. The differential harness passes with only D5-listed differences.

## Stage 5.10 - Cutover and Removal

- Make the lowered emitter the only emitter. Delete the legacy `ModuleEmitter` and every legacy-only helper, the D1 selector and feature, the `#[cfg(test)]` legacy recorder and `legacy_emissions`, and the transition tests that compare against legacy recordings (the per-family 4.3–4.6 comparisons, the Stage 3.5 queue comparison, and the 4.7 census). The plan-level validators and the new-emitter census keep their guarantees. Before deleting each transition test, check whether it asserts a plan-content fact that no validator re-checks. If it does, convert it into a lowering-only test instead of dropping it.
- Remove `typed: Box<TypedModule>` and `LoweredModule::typed`. `Lowerer::lower` still reads `&TypedModule`; only its output stops carrying it.
- Mechanical checks: in `staple-compiler/src/codegen*`, `rg` finds none of `TypedModule`, `typed_module`, `ResolvedModule`, `resolved()`, `staple_syntax::{Expression,Item,Pattern,CallExpression,ProductExpression,RepeatedProductExpression}`, `SyntaxId`, `DefaultHasher`, `active_type_substitutions`, `expression_type_overrides`, `specialization_queue`, `infer_type_parameters`, `substitute_type`, `contains_type_parameter`, `standard_function_name_matches`, or `{:?}` inside a symbol name.
- Update [TYPED_LOWERING_PLAN.md](TYPED_LOWERING_PLAN.md) and this file with what passed and the observed differences. Also record the Stage 6 hand-off: the `TypedModule` accessors that are now unused outside lowering, diagnostics, and tooling.

**Gate:** The full gate set (Contract 6) passes on the single emitter. Representative LLVM from before and after (the 5.2 corpus) shows no function-type changes and no duplicate instances. The CLI `--emit llvm`, `--emit object`, and `run` paths pass with the worktree standard library. The migration is complete; Stage 5.11 follows.

## Stage 5.11 - Fix the Mirrored Defects

This runs after the cutover, so each fix lands in one backend (D5). It is the only part of Stage 5 that intentionally changes behavior, and each fix is its own commit with fixtures that assert the corrected behavior. No separate plan is needed for the coroutine fix. The generic `Drop` fix should write a short design note in this section before it starts, because it changes checker semantics.

- **Completed-coroutine frame-binding leak (4.5).** Today a completed coroutine body never drops its droppable frame bindings. Only the cancel unwind drops them, through the pair plan's `unwind_drop`.
  - Add completion drops to `CoroutineFramePlan`, in plan order and bound to `DropGlue` like `unwind_drop`. Extend `expand_coroutine_codes`, `check_stage_4_5`'s re-expansion, `visit_types`, and the catalog snapshot to cover them.
  - Emit the drops on the body's normal-return path in `resume`, before the result is published. Use the frame cells' state (conditional cell drop) so a binding that was moved out, or never initialized, is not dropped.
  - Fixtures: a `Drop` impl that prints, held in a coroutine local that completes normally; one that is moved out before completion; one in a branch that never ran; a cancelled coroutine (the unwind path must still drop exactly once); nested and child-awaited coroutines.
- **Generic `Drop` implementations never selected (4.4).** Today both the checker (`TypedModule::drop_method_for`, `has_drop_implementation`) and lowering (`drop_implementation_for`) require the implementation's trait argument to equal the concrete type exactly, so `impl<T> Drop (Box T)` is accepted but never runs. This is a language-semantics fix, not only a code-generation fix:
  - Select `Drop` through trait resolution. In the checker, use the same header unification and conditional-bound discharge as other traits. In lowering, use `select_concrete_trait_method` with kind `DropMethod`, which yields an instance request that carries the implementation's substitutions.
  - Keep the checker and lowering in agreement. `type_needs_drop`/`concrete_needs_drop`, `is_copy_type`/`concrete_is_copy`, and the ownership checks that follow from them must all see the same set of droppable types. Add an agreement test over a fixture sweep, like the Stage 3.4 recomputation test.
  - Decide and record what happens with overlapping implementations (an exact implementation next to a generic one). Recommendation: follow the checker's existing implementation-selection rules; if those report ambiguity, report it here too rather than inventing a precedence.
  - Fixtures: generic `Drop` on a nominal wrapper at two instantiations (both drop, and each calls its own instance); a generic `Drop` with a conditional bound that holds for one argument and not another; nested generic droppables inside products and sums; a value moved out of a generic droppable; closures and coroutines capturing one.
  - Replace the Stage 4.4 `Box` fixture, which asserts the implementation is *not* selected, with one that asserts it is selected.
- Update [TYPED_LOWERING_PLAN.md](TYPED_LOWERING_PLAN.md), this file, and the 4.4/4.5 plans' latent-defect notes to say where each defect was fixed.

**Gate:** The new fixtures print the corrected drop output. The full gate set (Contract 6) passes. The checker/lowering drop-agreement test passes. **Only now may Stage 5 be marked complete.**

## Ordering and Parallelism

```text
5.1 → 5.2 → 5.3 → 5.4 → { 5.5, 5.7 } → 5.6 → 5.8 → 5.9 → 5.10 → 5.11
```

- 5.1 and 5.2 touch disjoint files (lowering versus `codegen.rs`) and can run in parallel. 5.3 needs both.
- 5.4 must come before everything else: every runnable fixture needs calls, IO resources, and closures.
- After 5.4, 5.5 (expressions and control flow) and 5.7 (structural and formatting) can proceed in parallel in separate files under `codegen/lowered/`. 5.6 needs 5.5's scope and loop hooks. 5.8 needs 5.6 (frame cleanup, finalizers, `unwind_drop`) and 5.4 (thunks, resources).
- Each substage appends to the differential corpus. It never removes an earlier substage's fixtures.
- 5.11's two fixes are independent of each other and can land in either order, but only after 5.10.
- Substages with a separate plan (5.4, 5.5, 5.6, 5.8) write that plan as their first step, following the Stage 4.x plan format (steps, gates, handoff), and link it from their section here.

## Stage Boundary and Definition of Done

- Stage 5 delivers a single LLVM emitter whose only compiler input is `LoweredProgram`, with deterministic, catalog-driven declaration and emission, and legacy parity on behavior and function types.
- Stage 5.11 then fixes the two mirrored defects (D5).
- Stage 5 does not remove `TypedModule` accessors that lowering, diagnostics, or tooling still use, and it does not add module-level phase documentation. Both are Stage 6.
- No concrete ABI change. Internal generated symbol names change only as D2 allows. The only intended behavior changes are the two 5.11 fixes: completed coroutines drop their frame bindings, and generic `Drop` implementations run.
- Mark Stage 5 complete in the main plan only when every Stage 5.10 and 5.11 gate passes; then identify Stage 6 as next.
