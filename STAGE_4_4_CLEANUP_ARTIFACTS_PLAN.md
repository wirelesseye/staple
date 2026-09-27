# Stage 4.4 Plan: Ownership Cleanup — Drop Glue, Finalizers, and Clone

**Status:** Step 1 complete. Steps 2-7 remain. Stage 4.3 is complete through `69b2441`: constructor-adapter and structural-method plans, the planned-callee mechanism (`PlannedInstance`/`PlannedArtifact`, `bind_artifact_plan_callees`, `check_planned_callees`, plan equality in the fixed-point re-check), and the legacy transition recorder in `codegen.rs`. Stage 4.3 already *requests* two Stage 4.4 key families with placeholder plans:

- `GcFinalizer::Payload` from `ManagedRef` constructor adapters;
- `DropGlue` from structural `MutateReplace`.

Stage 4.4 must fill those plans without re-keying them.

## Goal and boundary

Stage 4.4 makes every ownership-cleanup decision the legacy backend makes during emission an owned, keyed, validated lowered record:

- **Drop glue** for every concrete type the backend drops, as `DropGlue(CanonicalType)` plans that mirror `compile_drop_value` exactly.
- **GC finalizers** of all four subkinds (`Payload`, `Cell`, `ClosureEnvironment`, `Buffer`), mirroring `ensure_gc_finalizer`, `ensure_cell_finalizer`, `ensure_closure_finalizer`, and `ensure_buffer_finalizer`.
- **Owned bindings.** For each instance body and initializer, record the symbols the backend tracks for scope-exit cleanup (`track_symbol_ownership` and the owned-cell path in `allocate_binding_cell`), each with its concrete type and drop glue.
- **Site uses.** Every lowered site that drops a value, sets a finalizer, or clones buffer elements gets an `ArtifactUseSite` and a closure-phase use record, so Stage 5 emits the reference from the exact site.
- **`BufferClone`.** The intrinsic's element `Clone` method becomes an instance use. Its destination-buffer finalizer becomes an artifact use.

After Stage 4.4, the backend has no remaining need for `TypedModule::{drop_method_for, type_needs_drop, is_non_owning_symbol, is_coroutine_type, is_scheduler_type, is_wait_type, is_resolver_type, is_completion_token_type}`, `resolved().standard_trait("Clone")`, or `has_mutable_storage`/`is_borrowed_capture`/`requires_initialization_state` to decide cleanup. It also no longer needs the `Debug`-string/`DefaultHasher` finalizer cache.

Out of scope:

- **Coroutine frames** belong to Stage 4.5. That covers the `resume`/`cleanup` pair, the frame-binding cell drops inside `ensure_coroutine_codes`, and the closure finalizers for coroutine thunk environments. Stage 4.4 provides the key and plan builders Stage 4.5 reuses. Dropping a *`Coroutine` value* is in scope: legacy calls the frame's cleanup indirectly through the header slot, so it needs no `CoroutineCodes` reference.
- **Runtime symbol requirements** belong to Stage 4.6. That covers `free`, `__staple_sched_destroy`, the completion release functions, and `__staple_gc_set_finalizer`. Stage 4.4 records which runtime release each plan performs as a typed enum, and Stage 4.6 turns those into `LoweredRuntimeRequirements`.
- **Cleanup scheduling** belongs to Stage 5. That covers which exits run which drops (block end, match arm, logical, `break`/`continue`, `return`, `?` propagation) and runtime live flags. Ownership order is recorded here, but the exit schedule stays derivable from lowered structure.
- There is no backend change and no ABI change. Legacy emission remains the reference.

Line references are against `69b2441` and will drift; function names are authoritative.

## Legacy behavior to capture

### `compile_drop_value(value, T)`

The decision order matters and must be preserved exactly.

1. **User `Drop`.** If `drop_method_for(T)` returns an implementation, call it. The implementation is chosen by an exact `CheckedType ==` match on a one-argument `Drop` implementation, and legacy calls it only if it is already in `self.functions`, meaning an eager root. The call passes a null environment and a borrowed pointer. Then, if `T` is `Distinct`, drop the representation recursively. After this step, return.
2. **`Coroutine` opaque.** Load the cleanup function from the frame header's `CORO_CLEANUP_FN` slot and call it indirectly. Return.
3. **`Scheduler` opaque.** Call `__staple_sched_destroy`. Return.
4. **`Wait`, `Resolver`, or `CompletionToken` opaque.** Call `__staple_completion_wait_drop`, `__staple_completion_resolver_drop`, or `__staple_completion_token_release` respectively. Return.
5. Otherwise, match on the structure of `T`:
   - **`CString`:** call `free`.
   - **`Product`:** drop each element that needs drop, in **reverse** element order.
   - **`Sum`:** switch on the tag. In each alternative that needs drop, extract its payload and drop it.
   - **`Distinct`:** drop the representation.
   - **Anything else:** no-op.

Generic `Drop` implementations such as `impl<T> Drop (Box T)` are accepted by the checker. However, both `drop_method_for` and `type_needs_drop` match only an implementation whose argument is exactly the concrete type, so a generic implementation never matches a concrete type. This is a latent language gap. Stage 4.4 **mirrors it**, records it in the handoff, and does not fix it.

### Finalizers

| Legacy | Trigger | Body |
| --- | --- | --- |
| `ensure_gc_finalizer(payload)` via `build_ref_value` | Managed `Ref` allocation (constructor call or adapter) when `type_needs_drop(payload)` | Load the payload and drop it |
| `ensure_cell_finalizer(T)` via `allocate_binding_cell` | GC-allocated **captured** binding cell (`captured_cell_symbols`) when `type_needs_drop(T)` | Conditional cell drop: if cell state is `2`, drop the value and set the state to `0` |
| `ensure_closure_finalizer(closure, env)` via `build_capture_environment` | Fresh closure environment | For captures in **reverse** order, skip the capture if it requires initialization state, has mutable storage, is derived, or is borrowed. Otherwise drop it if its substituted type needs drop. |
| `ensure_buffer_finalizer(element)` via `compile_buffer_with_capacity` and `compile_buffer_clone` | Buffer allocation with a droppable element | Loop over `0..length` and drop each element |

### Other drop sites

| Legacy call site | Lowered fact that already exists |
| --- | --- |
| `bind_pattern_value` wildcard | Wildcard pattern whose concrete type needs drop |
| `compile_item` expression statement / `compile_top_level_item` | `LoweredExpressionStatementItem.drop_result` |
| `compile_assignment` (value, or conditional cell drop for cell places) | `LoweredAssignmentItem.drop_previous` |
| `compile_loop_expression` | `LoweredLoop.drops_body_result` |
| `drop_mutation_temporaries` | `LoweredCallArgument.drops_after_call` |
| Extern call C-string temporary (`compile_call_expression`) | Stage 2.5 explicit extern C-string temporaries |
| `IntrinsicFunction::Drop` | Intrinsic call target |
| `compile_string_from_c_string` (drops the source `CString`) | Intrinsic call target |
| `compile_completion_intrinsic` orphan drop | Intrinsic call target plus value type |
| `compile_structural_replace` | Already a 4.3 `MutateReplace.drop_previous` |
| `drop_owned_since` / `drop_all_owned` over `owned`/`owned_cells` | **Not recorded yet.** Legacy decides ownership during emission. |
| `compile_buffer_clone` | `standard_trait("Clone")` by name, plus `trait_method_code` for the element, plus the destination finalizer |

Stage 3.4 Step 4 already recomputes the concrete `drop_result`, `drop_previous`, `drops_body_result`, `drops_after_call`, and capture `drops_value`/`owns_value` facts on instance bodies. Stage 4.4 reads those facts and never recomputes them from templates.

## Design

### Drop-glue plan

```rust
pub(crate) struct DropGluePlan {
    pub value_type: CheckedType,        // identity (existing field)
    pub body: DropGlueBody,
}
pub(crate) enum DropGlueBody {
    Unexpanded,                          // request-time marker
    UserDrop { method: PlannedInstance, representation: Option<PlannedArtifact> },
    CoroutineCleanup,                    // indirect through the frame header slot
    RuntimeRelease(RuntimeRelease),      // Scheduler / Wait / Resolver / CompletionToken
    CStringFree,
    Product { fields: Vec<DroppedElement> },           // already in reverse drop order
    Sum { alternatives: Vec<DroppedAlternative> },     // tag order; only droppable ones listed
    Distinct { representation: PlannedArtifact },
}
pub(crate) enum RuntimeRelease { SchedulerDestroy, WaitDrop, ResolverDrop, CompletionTokenRelease }
pub(crate) struct DroppedElement { pub index: usize, pub value_type: CheckedType, pub glue: PlannedArtifact }
pub(crate) struct DroppedAlternative { pub index: usize, pub value_type: CheckedType, pub glue: PlannedArtifact }
```

Field names are open; the following rules are not.

- **Selection order.** The expander mirrors the legacy decision order above. Opaque classification uses `semantic_ids.{coroutine_type, scheduler_type, wait_type, resolver_type, completion_token_type}`, exactly as `concrete_needs_drop` does.
- **Only droppable types get glue.** A `DropGlue` key is requested only for a type where `program.concrete_needs_drop(T)` holds, and validation rejects any other key. A type that would take the legacy `_ => {}` no-op branch must never need drop. If such a type appears, it is a diagnostic, because `needs_drop` and the body disagree.
- **User-`Drop` selection.** Add `LoweredProgram::drop_method_for_concrete(T)`, which scans the owned trait-implementation catalog with the **same predicate** as `concrete_type_needs_drop` (one argument, argument equal to `T`, method from the implementation's method map). Put the predicate in one shared helper so that `needs_drop` and selection cannot drift. The method becomes a `Root` instance request with the implementation's template signature and no substitutions, kind `LoweredInstanceDependencyKind::DropMethod` (new). This replaces legacy's "must already be eager" dependency with an explicit edge. The instance deduplicates with the existing eager root.
- **Selection must be a function of the key.** `DropGlue` keys are `CanonicalType`, which drops display names and defaults, while selection uses `CheckedType ==`. Two checked types with the same canonical key but different selection results would silently alias to one plan. Two changes guard against this:
  - The expander computes the selection. The fixed-point re-check already recomputes the plan from the stored `value_type`.
  - Add a validator check: for every `DropGlue` artifact, re-run selection on each requester's original type (recorded on the edge origin; see Step 1) and require it to agree with the plan. If agreement cannot be proven cheaply, compare canonical forms in the selection predicate instead, and record that decision.
- **The representation after a user drop** is requested only when `concrete_needs_drop(representation)` holds. Legacy calls `compile_drop_value(representation)` unconditionally, but for a type that doesn't need drop that call is a no-op, so the behavior is identical. Record this equivalence in the step notes.
- **Recursion.** Product, Sum, and Distinct request nested `DropGlue` keys. Recursive nominal types terminate through key deduplication (the 4.2 contract). A type that reaches itself through a `Ref` does not need drop through the `Ref` (legacy `_ => {}` for `Ref`), so drop glue has no cycle through a `Ref`.

### Finalizer plans

```rust
pub(crate) enum GcFinalizerPlan {
    Payload { value_type: CheckedType, glue: Option<PlannedArtifact> },
    Cell { value_type: CheckedType, glue: Option<PlannedArtifact> },
    ClosureEnvironment { closure: FunctionInstanceId, captures: Vec<CheckedType>,
                         drops: Option<Vec<DroppedCapture>> },   // None = unexpanded
    Buffer { element: CheckedType, glue: Option<PlannedArtifact> },
}
pub(crate) struct DroppedCapture { pub index: usize, pub value_type: CheckedType, pub glue: PlannedArtifact }
```

- **`Payload`, `Cell`, and `Buffer`.** The glue is `None` only in the request-time marker form. A finalizer is requested only when its value or element needs drop (legacy gates on `type_needs_drop` before creating it), so every expanded finalizer has glue. Add an `is_expanded` arm for each subkind.
- **`ClosureEnvironment`.**
  - Keep the 4.1 key: the closure's instance ordinal plus the concrete types of **all** captures in environment order. That list determines the layout.
  - `drops` lists, in **reverse** capture order, the captures the finalizer drops. Take them from the closure construction's `LoweredClosureCapture { drops_value, value_type }` in the requesting body. That fact is Stage 3.4's recomputation of legacy's skip rules (initialization-state, mutable storage, derived, borrowed) plus `type_needs_drop`.
  - Because the finalizer depends on the construction site's capture facts, the expander must read them from the key's closure instance, not from the site. Add a lowered accessor that yields the closure instance's own capture metadata (`LoweredInstanceBody::captures`). Before relying on it, assert in a test that the drop decision agrees with every construction site's `drops_value`.
- **When a finalizer is requested:**
  - `Payload`: every managed `Ref` construction whose payload needs drop. That covers 4.3 adapters plus 4.4 scanner sites for direct `Ref` construction calls.
  - `Cell`: every captured-cell binding (`LoweredSymbol.captured_cell`, plus `LoweredBindingItem.cell`) whose concrete value type needs drop.
  - `ClosureEnvironment`: every closure construction with a `Fresh`, non-empty environment for which `build_capture_environment(.., install_finalizer = true)` installs one. Its gate is "some capture that is neither initialization-state nor borrowed has a substituted type that needs drop". The finalizer *body* additionally skips mutable-storage and derived captures, so a legitimately installed finalizer may drop nothing (`drops` empty). Mirror both the gate and the body rules. Do not derive the gate from `drops_value`, which applies the stricter body rule. Record the install-gate inputs explicitly, or add them to `LoweredClosureCapture` if Stage 3.4 does not already carry them. Coroutine thunk environments use `install_finalizer = false` and install their finalizers in `ensure_coroutine_codes`, which is Stage 4.5.
  - `Buffer`: `BufferWithCapacity` and `BufferClone` intrinsic sites whose element needs drop.

### Owned bindings (scope-exit ownership)

Legacy decides ownership during emission. `track_symbol_ownership` registers a symbol when all of the following hold: it is not `is_non_owning_symbol`, it has an SSA local value, and its substituted type needs drop. `allocate_binding_cell` registers an owned *cell* for a non-captured cell binding whose type needs drop. Stage 4.4 records this as data:

```rust
pub(crate) struct LoweredOwnedBinding {
    pub symbol: SymbolId,
    pub pattern: PatternId,                 // owner-local binding or `at` pattern that introduces it
    pub storage: OwnedStorage,              // Value | Cell
    pub value_type: CheckedType,            // concrete
    pub glue: ArtifactOrdinal,              // bound through the use record
}
```

- `LoweredInstanceBody::owned_bindings` and `LoweredProgram::initializer_owned_bindings` are stored in **registration order**, which is the pattern traversal order: function parameter pattern first, then body bindings in lowered evaluation order.
  - Registration order is legacy's `owned_order`. Stage 5 drops in reverse order from a scope's start mark, so this order is part of the contract.
  - Top-level initializer bindings are module globals. They are never owned (`locals` is empty for them). Only nested block locals inside initializer expressions qualify, so the scanner must not register globals.
- The concrete type comes from the owner's binding pattern (instance-local, already concrete). It never comes from `LoweredSymbol.value_type`, which is the template type.
- Ownership inputs are `LoweredCapture.non_owning`/the symbol catalog's non-owning fact, `SymbolStorage`, `LoweredBindingItem.cell`, `LoweredSymbol.captured_cell`, and `concrete_needs_drop`. Add a transition test that agrees the recorded set with legacy's `owned`/`owned_cells` for every emitted function (Step 6 recorder).

### Scanner and use sites

Stage 4.4 is the first substage with a real scanner. Following the 4.2 recipe, add these `ArtifactUseSite` variants (names open) and give each a `check_use_site` arm that proves the ID exists in the owner's arenas:

| Variant | Requests |
| --- | --- |
| `DiscardedResult(ItemId)` | `DropGlue(expression type)` |
| `ReplacedValue(ItemId)` | `DropGlue(place type)` |
| `LoopBodyResult(ExpressionId)` | `DropGlue(loop result type)` |
| `CallTemporary { call, argument }` | `DropGlue(argument type)` |
| `CStringTemporary(LoweredCallId)` | `DropGlue(CString)` |
| `WildcardDiscard(PatternId)` | `DropGlue(pattern type)` |
| `OwnedBinding(PatternId)` | `DropGlue(binding type)`, and also creates the `LoweredOwnedBinding` record |
| `CellFinalizer(PatternId)` | `GcFinalizer::Cell` |
| `ClosureEnvironment(LoweredCallableValueId)` | `GcFinalizer::ClosureEnvironment` |
| `RefConstruction(LoweredCallId)` | `GcFinalizer::Payload` |
| `DropIntrinsic(LoweredCallId)` | `DropGlue(argument type)` |
| `CStringConversion(LoweredCallId)` | `DropGlue(CString)` |
| `CompletionOrphan(LoweredCallId)` | `DropGlue(value type)` |
| `BufferAllocation(LoweredCallId)` | `GcFinalizer::Buffer` |
| `BufferCloneFinalizer(LoweredCallId)` | `GcFinalizer::Buffer` |
| `BufferCloneElement(LoweredCallId)` | **Instance** use of the selected `Clone` method, kind `CloneMethod` (new) |

- **Site order.** The scanner walks each owner in lowered evaluation order, which is the Stage 3.3 traversal order. At a site with several uses, it follows a documented per-site order. For example, `BufferClone`: element `Clone` first, then the finalizer.
- **Sites never need a separate kind for exit paths.** One `OwnedBinding` use covers every exit that drops that binding.
- **Buffer `Clone` selection** reuses 4.3's `select_concrete_trait_method` with `semantic_ids.clone_trait` and its first declared method. Clone has no default methods today, so the 4.3 review finding about `implementation_substitutions` diverging from the worklist's `trait_site_substitutions` does not bite here. Still, before the first use, add a unit test that resolves a `Clone` implementation whose generic parameter appears only in the implementation header (for example `impl<T where Clone T> Clone (Buffer T)`), or reconcile the two substitution recipes.
- **Registration.** The 4.4 scanner registers in `ProductionHooks::{scan_initializer, scan_instance}` in the 4.4 slot (4.3 has none). Its expanders register in the `DropGlue` and `GcFinalizer` arms, and `expands_body` reports both families. Extend `supports_planned_callees` to both families so `check_planned_callees` validates their callees against edges.

### Module layout

- New private `lower/cleanup_artifacts.rs` holds the scanner, `expand_drop_glue`, `expand_gc_finalizer`, the owned-binding collector, and `drop_method_for_concrete` (or put that last one next to `concrete_needs_drop` in `instance_resolution.rs` so the two share the predicate).
- `artifact_plan.rs` gets the plan types, `visit_callees` arms, and `is_expanded` arms.
- `artifact_closure.rs` gets the `ArtifactUseSite` variants and `check_use_site` arms.
- `instance_body.rs` and `lower.rs` get the owned-binding storage.

## Implementation sequence

### Step 1: Schema, use sites, and owned-binding storage

- Define the plan types, the marker forms, `visit_callees`/`is_expanded` arms, `supports_planned_callees` for both families, the new dependency kinds (`DropMethod`, `CloneMethod`), the `ArtifactUseSite` variants with exhaustive `check_use_site` arms, and `LoweredOwnedBinding` storage on instance bodies and initializers.
- Update the Stage 4.3 requesters (the constructor `ManagedRef` finalizer and `MutateReplace` drop glue) to the marker forms. Their keys and kinds do not change.
- Extend the normalized snapshot with owned bindings, uses, and plan bodies.
- **Gate:** placeholder expanders are still in place and the full suite passes. The snapshot shows marker plans. A corruption test proves that a use site pointing outside the owner's arena is diagnosed.

**Step 1 notes (complete):**

- **Drop-glue schema.** `DropGluePlan { value_type, body: DropGlueBody }` with `DropGlueBody::{Unexpanded, UserDrop { method: PlannedInstance, representation: Option<PlannedArtifact> }, CoroutineCleanup, RuntimeRelease(RuntimeRelease), CStringFree, Product { fields }, Sum { alternatives }, Distinct { representation }}`; `RuntimeRelease::{SchedulerDestroy, WaitDrop, ResolverDrop, CompletionTokenRelease}`; `DroppedElement`/`DroppedAlternative` name the field/alternative index, concrete type, and bound glue. Every variant except `Unexpanded` is non-empty, so `is_expanded`/`supports_planned_callees` are meaningful immediately.
- **Finalizer schema.** `GcFinalizerPlan::Payload`/`Cell`/`Buffer` now carry `glue: Option<PlannedArtifact>` and `ClosureEnvironment` carries `drops: Option<Vec<DroppedCapture>>`; `Option::None` is the request-time marker. `DroppedCapture` names the capture index, concrete type, and bound glue in reverse capture order.
- **Callees.** `visit_callees`/`visit_callees_mut` walk both families in request order (user-drop method then representation; product fields; sum alternatives; distinct representation; finalizer glue; capture drops), and `supports_planned_callees` reports both. `check_planned_callees` skips a plan that is still a request-time marker, so the pre-expander snapshot stays valid while the schema is ready.
- **Dependency kinds.** `LoweredInstanceDependencyKind::{DropMethod, CloneMethod}` added with descriptions.
- **Use sites.** The sixteen Stage 4.4 `ArtifactUseSite` variants are defined with exhaustive owner-aware `check_use_site` arms that resolve every site ID in the owning body's own arenas (initializer sites index the program's template arenas). `closure_use_site_outside_the_owner_arenas_is_diagnosed` proves an out-of-arena site is reported.
- **Owned bindings.** `LoweredOwnedBinding { symbol, pattern, storage: OwnedStorage::{Value, Cell}, value_type, glue: Option<ArtifactOrdinal> }` is stored in registration order on `LoweredInstanceBody::owned_bindings`; `LoweredProgram::initializer_owned_bindings` is sized and reset by `close_artifact_catalog`. The collector that fills them (and binds `glue` through the `OwnedBinding` use record) is Step 4.
- **Requesters.** The 4.3 constructor-adapter `GcFinalizer::Payload` and structural `MutateReplace` `DropGlue` requests now use the marker forms; keys and dependency kinds are unchanged.
- **Snapshot.** The closure snapshot renders plan bodies, instance uses, and owned bindings.
- **Gate:** met. Placeholder expanders are still registered, the whole workspace suite passes (1230 tests), the snapshot shows marker plans, and the new corruption test diagnoses a use site outside the owner's arenas.

### Step 2: Drop glue

- Implement the shared drop-implementation predicate, `drop_method_for_concrete`, and `expand_drop_glue`, following the decision order above.
- Register the `DropGlue` expander. Validation rejects a `DropGlue` key whose type does not need drop.
- Fixtures:
  - a user `Drop` on a nominal type whose representation also needs drop (`CString` field);
  - a user `Drop` on a nominal type whose representation does not;
  - nested products and sums with mixed droppable fields (check reverse order and only-droppable alternatives);
  - `CString`;
  - each runtime opaque type (`Coroutine`, `Scheduler`, `Wait`, `Resolver`, `CompletionToken`);
  - a recursive nominal type through a sum;
  - drop glue requested from two generic instantiations (two keys);
  - a generic `Drop` implementation, asserting that it is **not** selected, which mirrors legacy.
- **Gate:** every 4.3-requested `DropGlue` is now expanded. Every plan's user-drop selection agrees with `TypedModule::drop_method_for`, and its needs-drop gating agrees with `type_needs_drop`, both checked in a transition test. Closure converges; record the round and growth maxima.

**Step 2 notes (complete):**

- **Module.** New private `lower/cleanup_artifacts.rs` owns `expand_drop_glue`; `ProductionHooks::expand` dispatches the `DropGlue` arm to it and `expands_body` reports the family, so validation rejects any drop-glue plan left `Unexpanded`.
- **Selection.** `LoweredProgram::drop_method_for_concrete` and `concrete_type_needs_drop` now share `drop_implementation_for`: the one-argument `Drop` implementation whose trait argument equals the concrete type exactly (mirroring legacy `has_drop_implementation`), with the method taken from the implementation's method map. The selected method becomes a `Root` instance request with the template signature and no substitutions, kind `LoweredInstanceDependencyKind::DropMethod` (new). A generic `Drop` implementation never matches a concrete type, exactly as legacy `drop_method_for` does; the `Box T` fixture asserts it is not selected.
- **Decision order.** `drop_glue_body` checks user `Drop` first, then the coroutine cleanup, then `Scheduler`/`Wait`/`Resolver`/`CompletionToken` runtime releases (via the new `LoweredProgram::runtime_opaque_kind`, shared with `concrete_needs_drop`), then `CString` (`free`), then the structural product (reverse element order, droppable fields only), sum (tag order, droppable alternatives only), and `Distinct` representation. Each nested cleanup is a `DropGlue` request in that same order, so a recursive nominal type terminates through 4.2 key deduplication. `CStringFree` is the `free` release; Stage 4.6 turns `RuntimeRelease` and `CStringFree` into runtime requirements.
- **Needs-drop gating.** A `DropGlue` key whose type `concrete_needs_drop` rejects is an expander diagnostic; every requester already gates on `concrete_needs_drop`. The representation drop after a user `Drop` is requested only when `concrete_needs_drop(representation)` holds: legacy calls `compile_drop_value(representation)` unconditionally, but that call is the `_ => {}` no-op for a type that does not need drop, so the recorded plans are behaviorally identical.
- **Canonicalizing coroutine types (new).** `Coroutine{E} T` encodes its concrete effect row as a function type with `Error` parameter and result (`effect_substitution_type`), which `CanonicalType::convert` previously rejected. `CheckedType::Function` values in that exact shape now canonicalize as a never-parameter marker that keeps the canonical effect row, so coroutine/task type arguments stay part of artifact and instance keys. Existing key encodings are unchanged (no previously-passing key contained an error-charged function), so `SPECIALIZATION_KEY_ENCODING_VERSION` stays 2.
- **Fixtures.** `cleanup_artifacts` adds a scripted-closure fixture covering every `DropGlueBody` variant: user `Drop` with a droppable `CString` representation (`Handle`) and without one (`Resource`), a represented distinct with no user `Drop` (`Wrapped`), mixed and nested products (reverse order, droppable fields only), a mixed sum (tag order), a recursive nominal type through a `Ref` sum, `CString`, all five runtime opaque kinds, two generic `Box` instantiations, and the generic-`Drop`-not-selected case; the same test sweeps every expanded key against `TypedModule::type_needs_drop`/`drop_method_for`. `graph_validation` adds `drop_glue_plans_agree_with_the_typed_module`, which lowers real homogeneous-pair mutation fixtures (`MutateIndex` requires homogeneous products) and applies the same agreement sweep to naturally requested keys.
- **Measurements.** The transition fixtures request 10 drop-glue plans; closure converges in 1 round with maximum growth 5 (the small generic-box fixture: 1 round, growth 1).
- **Gate:** met. All 4.3-requested glues expand, the workspace suite passes 1233 tests, and selection/gating agree with the typed module on every expanded key.

### Step 3: Finalizers

- Implement `expand_gc_finalizer` for all four subkinds. Add the closure-instance capture accessor and the agreement assertion.
- Mirror the `build_capture_environment` install gate, which differs from the body's skip rules as described in the design, and add a fixture where the gate fires but the body drops nothing: a droppable capture with mutable storage.
- Fixtures:
  - a `Ref` constructor call and adapter over droppable and `Copy` payloads;
  - a captured mutable binding with a droppable value (cell finalizer), plus a non-captured one (no finalizer);
  - closures capturing droppable values by value, borrowed captures, mutable/cell captures, and derived captures (all skip cases), in two generic instantiations;
  - buffers of droppable and `Copy` elements.
- **Gate:** every finalizer plan is expanded with bound glue. The finalizer set agrees with legacy's `gc_finalizers` population (Step 6).

### Step 4: Scanner and owned bindings

- Implement the scanner over instance bodies and initializers for every site in the table, and the owned-binding collector. Requests for uses go through `ClosureRequest { use_site: Some(..) }`, so the engine records uses and edges.
- Fixtures:
  - every site kind at least once, in both an instance and an initializer where legal;
  - owned parameters (moved versus borrowed/non-owning);
  - `let` in nested blocks, match arms, loop bodies, and `?` propagation;
  - owned cells versus captured cells;
  - an initializer whose top-level bindings are not owned but whose block locals are.
- **Gate:** the use/edge agreement validator passes over the whole standard library. The owned-binding order is deterministic across repeated lowering.

### Step 5: `BufferClone`

- Add the `BufferCloneElement` instance use (the `Clone` method selected through `select_concrete_trait_method`) and the `BufferCloneFinalizer` artifact use.
- Add the header-only generic parameter test from the design above.
- Fixtures: cloning a buffer of `Copy` elements (blanket `impl<T where Copy T> Clone T`), of a nominal type with an explicit `Clone`, and of a nested `Buffer` (the `Clone (Buffer T)` implementation), each with droppable and non-droppable elements.
- **Gate:** the selected `Clone` instance matches legacy `trait_method_code(Clone, [element])`.

### Step 6: Legacy transition comparison

Extend the `#[cfg(test)]` recorder in `codegen.rs`.

- **Record** every `compile_drop_value` entry with its concrete type, the branch it took (user drop plus function id, coroutine, runtime release name, `CString`, product, sum, distinct, or no-op), and its nested calls in order.
- **Record** every finalizer creation with its kind and legacy key inputs: payload/cell/element type, or closure `FunctionId` plus substituted capture types and the dropped capture indices.
- **Record** every `track_symbol_ownership` and owned-cell registration per emitted function, in order.
- **Record** each `compile_buffer_clone` `Clone` selection.
- **Observe** the constructor adapter's `finalizer_set` from the actual `set_gc_finalizer` call instead of recomputing `type_needs_drop`. This closes 4.3 review finding 2's recorder gap.

The transition test then requires:

- the set of legacy drop types equals the set of `DropGlue` keys, bidirectionally, compared canonically;
- each type's branch, user-drop function, and nested order match its plan;
- each legacy finalizer matches exactly one plan, and vice versa, with closure finalizers mapped through `instance.template` plus concrete captures;
- per emitted function, legacy's owned registrations match `owned_bindings` in order and storage kind;
- each buffer-clone selection matches its instance use.

Fixtures must be non-vacuous. Assert coverage of every `DropGlueBody` variant, every finalizer subkind, and every use-site kind.

While extending the recorder, also close 4.3 review finding 2: compare `IndexSwitch`/`IndexLoad`/`MutateReplace`/`IntoIterator` plan contents (elements, coercions, lengths, `drop_previous` presence) against recorded legacy decisions.

**Gate:** the transition test passes on the representative fixtures and on a program that exercises the standard library's `List`, `Buffer`, formatting, and coroutine/task values.

### Step 7: Gates and handoff

- Run `cargo fmt --all -- --check`, `cargo check --workspace`, `cargo test --workspace --quiet`, and `git diff --check`. Run the CLI `--emit llvm`, `--emit object`, and `run` paths with the worktree standard library.
- Record in this file:
  - the final plan schemas and use-site table;
  - the owned-binding contract, including registration order and what Stage 5 must still derive (the exit schedule and live flags);
  - the builders Stage 4.5 reuses for coroutine frames (`DropGlue` requests, `GcFinalizer::{Cell, ClosureEnvironment}`);
  - the `RuntimeRelease`/`CStringFree` hand-off to Stage 4.6;
  - the generic-`Drop` language gap;
  - the observed closure maxima.
- Update [STAGE_4_GENERATED_ARTIFACTS_BREAKDOWN.md](STAGE_4_GENERATED_ARTIFACTS_BREAKDOWN.md) and [TYPED_LOWERING_PLAN.md](TYPED_LOWERING_PLAN.md).

## Risks

- **Ownership parity is the highest risk.** `track_symbol_ownership` depends on `environment.locals` membership, which is an emission-time fact: SSA local versus cell versus global versus parameter pointer. Step 6's per-function owned-registration comparison is the detector. Any mismatch is fixed in the collector, not in the backend.
- **Canonical versus checked equality** in drop selection. See the design. If the fixed-point re-check or the transition test ever shows two checked types sharing a key with different selections, switch the predicate to canonical comparison and make the equivalent change in `concrete_type_needs_drop`, keeping `TypedModule` agreement in tests.
- **Closure finalizer identity.** The key uses the closure instance plus capture types, but the drop decision comes from capture facts. If two construction sites of the same closure instance ever disagree on `drops_value`, the key is too coarse. The Step 3 assertion detects this. The fix would be adding the drop mask to the key, which bumps the encoding version.
- **Growth.** Drop glue fans out per concrete field type. Record maxima and confirm the 4.2 growth budget stays far above them for the standard library.
- **Snapshot churn.** Many new uses, edges, and artifacts will appear. Stage 3 and 4.3 prefix ordinals must be unchanged, and treat every other diff as review-required.

## Stage boundary

- Stage 4.4 delivers expanded drop-glue and finalizer plans, the owned-binding records, every cleanup and clone site use, and the legacy transition comparison for cleanup. The catalog is closed for these families relative to legacy emission, except for coroutine frame internals, which are Stage 4.5.
- Stage 4.4 does not change emission, add runtime requirement records (Stage 4.6), or schedule exits (Stage 5).
