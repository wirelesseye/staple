# Stage 4.5 Plan: Coroutine Resume/Cleanup and Reactive Runners

**Status:** In progress. Steps 1-5 are complete and no gap fixture is ignored any more. Step 1 added the complete plan schema for the four families (`CoroutineCodesPlan { body, frame }`, `CoroutineFramePlan`, `CoroutineFrameBinding`, `CoroutineResourceSlot`, `ReactiveRunnerPlan { owner, site, body }`, `ReactiveRunnerBody`, `RunnerResourceSlot`), the four new `ArtifactUseSite` variants with exhaustive owner-aware `check_use_site` arms, and the two gap fixtures. Step 2 made the scan real for coroutines: `lower/coroutine_artifacts.rs` expands every `CoroutineCodes` plan from the body thunk's instance-local plan and body, the `CoroCreation` scanner arm requests one pair per creation in instances and initializers, and the family-neutral owner walker carries the coroutine hook. Step 3 expanded the three runner families and installed callback/evaluator finalizers from the owner's operation and callback records; the walker also gained the reactive hook and now descends into `await` operands. Step 4 extended the legacy recorder so `resume` state-0 registrations attribute to the body thunk, excluded frame bindings from the owned-binding collector, closed gap 2, and recorded the completed-body frame-binding leak as a mirrored latent defect. Step 5 added the plan and creation-use validators with four corruption tests. Steps 6-7 remain. Stage 4.4 is complete through `1948dcc`: expanded `DropGlue` and `GcFinalizer` plans, owned-binding records, every cleanup and clone site use, and the legacy cleanup transition comparison (whose drop-type set now walks nested drop calls). Stage 4.1 already defined the four key families this stage fills, and every request originally carried its placeholder plan:

- `CoroutineCodes(CoroutineCodesKey { body: InstanceOrdinal })` with `CoroutineCodesPlan { body }`;
- `ReactionRunner`, `UntilRunner`, and `DerivedRunner`, each `ReactiveRunnerKey { owner: ArtifactSiteOwner, site: ArtifactSite }` with `ReactiveRunnerPlan { owner, site }`.

Those were the Stage 4.1 placeholders. Stage 4.5 fills them without re-keying them, and the scanner now requests every one of them.

## Goal and boundary

Stage 4.5 makes every function the legacy backend generates for coroutines and reactive operations an owned, keyed, validated lowered record:

- **Coroutine codes.** One `resume`/`cleanup` pair per coroutine body-thunk *instance*, with a plan recording every input `ensure_coroutine_codes` and `coroutine_frame_layout` read from `TypedModule`: frame bindings, result type, awaited result types, resume-state count, `Wait`/`until` cancellation states, the deferred resource bundle, captures, the capture-environment finalizer, and the frame-binding drops the cancel unwind performs.
- **Reactive runners.** One `ReactionRunner` per `reaction` operation, one `UntilRunner` per `until` operation, and one `DerivedRunner` per derived creation, each keyed by owner plus lowered site. Each plan records the callback's concrete closure type and the payload slot order and pass modes the runner loads.
- **Site uses.** Every `coro` creation and every reaction/`until`/derived operation gets an `ArtifactUseSite` and a closure-phase use record, so Stage 5 emits each reference from the exact site.
- **Two Stage 4.4 gaps**, found while planning this stage (see "Carried-over gaps"):
  - the closure-environment finalizers for reactive callback thunks and derived evaluators;
  - coroutine-body ownership parity.

After Stage 4.5, the backend has no remaining need for these to emit coroutine or runner code:
- `TypedModule::{coroutine_plan, implicit_thunk_for, derived_evaluator, type_of_function}`;
- the `coroutine_codes` cache;
- the syntax-keyed `__staple_coro_{SyntaxId}`, `__staple_reaction_runner_{SyntaxId}`, and `__staple_until_runner_{SyntaxId}` names, and the evaluator-keyed `__staple_derived_runner_{FunctionId}` name.

Out of scope:

- **Runtime symbol requirements** belong to Stage 4.6. That covers `__staple_reaction_create`, `__staple_derived_create`, `__staple_batch_begin`/`end`, `__staple_until_resume`/`cleanup`, `__staple_completion_complete`/`abandon`, `__staple_gc_unregister_root`, the scheduler and drive entry points, and `llvm.trap`. Stage 4.5 plans name no runtime symbol. The plan's shape determines which runtime calls a body makes, and Stage 4.6 records those as requirements.
- **Frame and payload layout** stays in the backend: LLVM struct types, `CORO_*` header indices, store sizes, the pending-slot byte width, and binding-cell shapes (including signal metadata). Plans record the concrete `CheckedType`s and orders the backend lays out, never sizes.
- **Await sites** need no artifact use. Legacy `compile_coroutine_await`, `compile_external_await`, and `compile_coroutine_drive` reach a child's code only through its frame header slots (`CORO_RESUME_FN`, `CORO_CLEANUP_FN`), never a direct reference. Stage 3's `AwaitChildPlan` binding already records the child plan, and 4.5 keeps it unchanged.
- **Batch** has no runner. `compile_batch` brackets its callback with runtime calls only. Its callback environment finalizer *is* in scope (gap 1).
- There is no backend change and no ABI change. Legacy emission remains the reference.

Line references are against `1948dcc` and will drift; function names are authoritative.

## Legacy behavior to capture

### `ensure_coroutine_codes(body_syntax)`

The function is cached in `coroutine_codes` by the `coro` body `SyntaxId`. It reads `typed_module.coroutine_plan(body_syntax)`, `implicit_thunk_for(body_syntax)`, and `coroutine_frame_layout(body_syntax)`, then emits:

1. **`resume(frame) -> {i8, ptr}`.**
   - Unpack the deferred-effect resource bundle from `CORO_RESOURCES` when `deferred_effects.resources` is non-empty. Each field is loaded through a pointer when `resource.mutable || !is_copy_in_function(value_type, None)`; otherwise it is stored by value.
   - Bind captures from `CORO_CAPTURE_ENV` (`bind_environment_captures`).
   - Point every frame binding at its frame cell.
   - Check cancellation: when a task record is present and its cancel flag is set, and `state <= resume_points`, take the unwind path. Otherwise switch on state to `state.0..=state.{resume_points}`; `DONE`/`FREED` return "done, no value", and any other state traps.
   - **Cancel unwind.**
     - When the frame is parked on a `Wait` state, call `__staple_completion_abandon` on the stashed child record.
     - When it is parked on an `until` state, call the child's `cleanup` indirectly through its header slot.
     - If the thunk has **any** captures and `state == 0`, call the capture-environment finalizer (`ensure_closure_finalizer(thunk)`) on the environment. This is gated on non-empty captures, *not* on the closure install gate, so the finalizer may drop nothing.
     - Conditionally drop every frame binding whose substituted type needs drop (`compile_conditional_cell_drop`). **Legacy iterates `layout.cell_fields`, a `HashMap`, so this drop order is not deterministic.**
     - Mark the frame `DONE` and return `CANCELLED`.
   - **State 0** compiles the thunk body inline (`compile_expression(&thunk.body)`), not as a separate function. On a normal return it runs `drop_all_owned`, stores the result through `CORO_RESULT_PTR`, and marks the frame `DONE`.
2. **`cleanup(frame)`.**
   - If the state is `FREED`, return.
   - If `state == 0` and the thunk has captures, call the same capture-environment finalizer.
   - Unregister the frame's GC root, mark it `FREED`, and return.

`coroutine_frame_layout` builds the frame as:
- the ten header fields;
- one binding cell per `frame_bindings` symbol, in plan order (`compile_binding_cell_type`);
- the result field (`result_type`);
- when `resume_points > 0`, a pending-result byte array sized to the largest `await_result_types` store size.

`compile_coro_expression` then:
- calls `ensure_coroutine_codes`;
- builds the capture environment with `install_finalizer = false`;
- GC-allocates the frame, stores the header (state 0, `resume`, `cleanup`, the environment, the result pointer), and registers the frame as a GC root region.

**Latent defect.** Because the cache key is the body `SyntaxId`, a generic function containing `coro` instantiated at two types reuses the first instantiation's pair, which was compiled under the first `active_type_substitutions`. Stage 4.5 keys by body instance and must **not** reproduce this aliasing. The transition comparison explains the difference instead (Step 6).

### Reactive runners

| Legacy | Trigger | Name / cache | Runner body | Payload |
| --- | --- | --- | --- | --- |
| `compile_reaction` | `reaction` intrinsic call | `__staple_reaction_runner_{call SyntaxId}`, `add_function` with **no cache** (one runner per emission; LLVM suffixes duplicates) | Load the callback closure and each resource slot; indirect-call the closure code with `(environment, resources…)` | `{callback closure, resource…}`; a resource slot is a pointer when `mutable \|\| !is_copy_in_function(value_type, function_id)`, otherwise the value |
| `compile_until` → `emit_until_runner` | `until` intrinsic call | `__staple_until_runner_{call SyntaxId}`, **reused by name** (same latent aliasing as coroutines: the first predicate type wins) | If the completion is unresolved, indirect-call the predicate; if the result's `Bool` tag is `True` (alternative 0), complete the completion | fixed `{code, env, completion, scope}` pointers |
| `compile_derived_create` | derived binding creation | `__staple_derived_runner_{evaluator FunctionId}`, `add_function` with **no cache** | Load the evaluator closure, indirect-call it with its environment, and store the result through the output pointer | `{evaluator closure, output ptr}`; an evaluator with resources is a backend error |

Each runner calls its callback **indirectly** through the closure value, so a runner names no source-function instance. The callback's thunk instance is already bound by Stage 3 at `LoweredBindingSite::ReactiveCallback` or `DerivedEvaluator`.

Every callback closure is built with `build_closure`, which calls `build_capture_environment(.., install_finalizer = true)`. The reaction, batch, `until`, and derived evaluator thunks are included. So a thunk with a droppable, install-gated capture gets a `GcFinalizer::ClosureEnvironment` finalizer at the site.

### Carried-over gaps from Stage 4.4

1. **Reactive callback environment finalizers are not requested.**
   - The 4.4 scanner's `walk_callable_value` covers `LoweredCallableValue` closures only. It never visits `LoweredReactiveCallback` records (reaction, batch, `until`) or `DerivedCreate` evaluators.
   - So a reactive thunk capturing a droppable value has a legacy finalizer and no plan.
   - The 4.4 transition fixtures pass only because none of them has such a capture.
   - Stage 4.5 adds these requests at the reactive sites.
2. **Coroutine-body ownership was never compared.**
   - `legacy_record_owned` attributes a registration only when `environment.function_id` matches the function being emitted. `resume` uses `FunctionEnvironment::default()` (no `function_id`), so no registration inside a coroutine body was ever recorded.
   - Meanwhile the 4.4 collector registers the thunk instance's bindings as it would any function body. In legacy, every `frame_bindings` symbol is a pre-seeded frame cell, so `allocate_binding_cell` returns early and `track_symbol_ownership` finds no local: frame bindings are never owned.
   - They are dropped only on the cancel unwind. Whether a completed body leaks a droppable frame binding must be confirmed, then mirrored and recorded as a latent defect.
   - Stage 4.5 reconciles the collector for coroutine thunk instances and extends the recorder to cover `resume`.
3. **Silent skip in initializer closure resolution.**
   - `walk_callable_value` returns `Ok(())` when an initializer closure's resolved key is not interned.
   - Stage 4.5 reuses the same resolution for initializer `coro` and reactive sites. Make a missing instance a diagnostic in both places.

## Design

### Coroutine-codes plan

```rust
pub(crate) struct CoroutineCodesPlan {
    pub body: FunctionInstanceId,                // identity (existing field)
    pub frame: Option<CoroutineFramePlan>,       // None = request-time marker
}
pub(crate) struct CoroutineFramePlan {
    pub result_type: CheckedType,
    pub resume_points: usize,
    pub frame_bindings: Vec<CoroutineFrameBinding>, // plan order = frame cell order
    pub await_result_types: Vec<CheckedType>,
    pub wait_await_states: Vec<usize>,
    pub until_await_states: Vec<usize>,
    pub resources: Vec<CoroutineResourceSlot>,       // deferred effect row order
    pub captures: Vec<CheckedType>,                  // thunk capture order
    pub capture_finalizer: Option<PlannedArtifact>,  // GcFinalizer::ClosureEnvironment
}
pub(crate) struct CoroutineFrameBinding {
    pub symbol: SymbolId,
    pub value_type: CheckedType,
    pub unwind_drop: Option<PlannedArtifact>,        // DropGlue when the type needs drop
}
pub(crate) struct CoroutineResourceSlot {
    pub resource: CheckedResource,                   // concrete
    pub indirect: bool,                              // mutable || !concrete_is_copy
}
```

Field names are open; the following rules are not.

- **Source of truth.** The expander reads the key's body instance:
  - the thunk's instance-local plan (`body.plans`, entry `body.plan_template`, already substituted by Stage 3.4);
  - its `captures()`;
  - the concrete types of its frame-binding symbols, from the instance's own binding patterns, reusing the 4.4 collector's symbol-to-concrete-type lookup.
  
  It never reads the template plan that `LoweredCoro.plan` indexes, and never `TypedModule`. A body instance without a local plan, or a frame binding with no concrete type, is a diagnostic.
- **`capture_finalizer`** is requested exactly when the thunk has **any** captures. That mirrors legacy's `!thunk.captures.is_empty()` gate, not the closure install gate. The finalizer reuses the 4.4 `GcFinalizer::ClosureEnvironment` key (thunk instance ordinal plus ordered concrete capture types) and plan builder, so a legitimately requested finalizer may drop nothing.
- **Frame-binding drops.** `unwind_drop` is `Some` exactly when `concrete_needs_drop(value_type)`, and is requested through `request_drop_glue` in `frame_bindings` order. Legacy's order is `HashMap` order. The plan fixes plan order, the transition test compares these drops as a set, and the handoff records that Stage 5 must emit plan order.
- **Resource pass mode.** `indirect = resource.mutable || !program.concrete_is_copy(value_type)`. Legacy uses `is_copy_in_function(value_type, None)` on the substituted type. Step 2 checks agreement for every recorded bundle, and a disagreement is fixed in the plan, not in the backend.
- **Plan callees** are the capture finalizer (when present), then each frame binding's `unwind_drop` in order. Extend `visit_callees`/`visit_callees_mut`, `supports_planned_callees`, and `is_expanded` (`frame.is_some()`), and set `expands_body` for `CoroutineCodes`.
- **The body is not re-planned.** State 0 of `resume` is the body instance's own lowered body. Its cleanup sites, owned bindings, uses, and edges are already recorded on that instance by 4.4 (after the gap-2 reconciliation). The pair plan references the instance and adds only the frame facts.

### Reactive-runner plan

```rust
pub(crate) struct ReactiveRunnerPlan {
    pub owner: ArtifactSiteOwner,      // identity (existing fields)
    pub site: ArtifactSite,
    pub body: ReactiveRunnerBody,
}
pub(crate) enum ReactiveRunnerBody {
    Unexpanded,
    Reaction { callback_type: CheckedFunctionType, resources: Vec<RunnerResourceSlot> },
    Until { predicate_type: CheckedFunctionType },
    Derived { evaluator_type: CheckedFunctionType, output_type: CheckedType },
}
pub(crate) struct RunnerResourceSlot { pub resource: CheckedResource, pub indirect: bool }
```

- **Keys.** The existing families and sites stay:
  - `ReactionRunner` at `ArtifactSite::Callback(callback)`;
  - `UntilRunner` at `ArtifactSite::Callback(predicate)`;
  - `DerivedRunner` at `ArtifactSite::Operation(operation)`.
  
  The owner is the initializer or the instance ordinal whose arena holds the site. The same template site in two instances gives two keys, which is the point of the 4.1 decision.
- **Types.** The expander takes each callback type from the owner-local reactive callback's substituted `function_type`, or for derived, from the evaluator thunk instance's signature. The derived `output_type` is the evaluator's result type. A derived evaluator with resources is a diagnostic, mirroring legacy.
- **Pass modes.** A reaction resource slot is indirect when `mutable || !concrete_is_copy`. The `until` payload is fixed-shape, so its plan records only the predicate type. The expander diagnoses a predicate whose result is not `Bool`.
- **Runners have no planned callees.** Every call is indirect or a runtime surface (4.6). `supports_planned_callees` is `false` for the three runner families, and `is_expanded` is `body != Unexpanded`.

### Scanner and use sites

The 4.5 scanner registers in `ProductionHooks::{scan_initializer, scan_instance}` after the 4.4 scanner, keeping the documented 4.3 → 4.4 → 4.5 → 4.6 composition. It walks each owner in lowered evaluation order, reusing the 4.4 `CleanupWalker` traversal. Promote the walker's visitor to a family-neutral one rather than copying the traversal.

| Variant (names open) | Requests | Site order |
| --- | --- | --- |
| `CoroCreation(LoweredCoroId)` | `CoroutineCodes { body }` | at the `Coro` expression |
| `ReactiveCallbackEnvironment(LoweredReactiveCallbackId)` | `GcFinalizer::ClosureEnvironment` when the install gate fires (gap 1) | before the runner, for reaction/batch/`until` |
| `DerivedEvaluatorEnvironment(LoweredReactiveOperationId)` | `GcFinalizer::ClosureEnvironment` when the install gate fires (gap 1) | before the runner |
| `ReactiveRunner(LoweredReactiveOperationId)` | `ReactionRunner`, `UntilRunner`, or `DerivedRunner` | after the callback environment |

- **Body instance of a `coro`.** In an instance owner, take it from the `LoweredBindingSite::Coro(id)` binding. In an initializer owner, which has no binding table, resolve the thunk with the Stage 3.3 recipe: a `Root` request with the thunk template, its signature with the plan's deferred effects, and no substitutions. A key that is not interned is a diagnostic (gap 3).
- **Reactive operations** are reached through `LoweredCall.reactive` (reaction, batch, `until`, snapshot, scope) and `LoweredBindingItem.reactive` (derived creation). The walker must reach every one exactly once. Signal operations, snapshot, scope, and batch request no runner.
- **Install gate for reactive callbacks.** Use the same predicate as 4.4 (`!requires_initialization_state && !borrowed && concrete_needs_drop`), evaluated over the callback thunk instance's `captures()`. `LoweredReactiveCallback.captures` carries no concrete types. The finalizer key and plan reuse the 4.4 builder unchanged.
- **Check arms.** Each new variant gets an owner-aware `check_use_site` arm that resolves its ID in the owner's arenas.

### Coroutine-body ownership (gap 2)

- Extend the Step 6 recorder so `resume` emission attributes registrations to the body thunk: set `function_id` and the legacy function key while compiling state 0. Then compare the thunk instance's `owned_bindings` against it like any other emitted function.
- Change the 4.4 owned-binding collector so that, for an instance with a `plan_template`, a symbol in the local plan's `frame_bindings` registers **no** owned record. It is a frame cell, dropped only through the pair plan's `unwind_drop`. Every other binding in the body (parameters, match-arm and nested pattern bindings that are not frame bindings) keeps the 4.4 rules.
- Confirm with a fixture whether a completed coroutine drops its droppable frame bindings. If it does not, record the leak as a latent defect in the handoff and mirror it; Stage 4.5 does not fix it.

### Module layout

- New private `lower/coroutine_artifacts.rs` holds the 4.5 scanner arm, `expand_coroutine_codes`, and `expand_reactive_runner`.
- `artifact_plan.rs` gets the plan types, `visit_callees`, `is_expanded`, and `supports_planned_callees` arms.
- `artifact_closure.rs` gets the use-site variants, `check_use_site` arms, the `ProductionHooks` scanner composition, and the expander and `expands_body` arms.
- `cleanup_artifacts.rs` gets the family-neutral walker and the frame-binding exclusion in the owned-binding collector.

## Implementation sequence

### Step 1: Schema, use sites, and failing gap fixtures

- Define the plan types with their marker forms, the `visit_callees`/`is_expanded`/`supports_planned_callees` arms, the four use-site variants with exhaustive `check_use_site` arms, and the snapshot rendering.
- Before any fix, add two currently failing tests that prove gaps 1 and 2 exist. Mark them `#[ignore]` with a reason until Steps 3 and 4 land:
  - a transition fixture with a `reaction` whose thunk captures a droppable value;
  - a coroutine-body ownership fixture.
- **Gate:** placeholder expanders are still registered and the suite passes. A corruption test proves each new use-site arm diagnoses an out-of-arena ID.

**Step 1 notes (complete):**

- **Coroutine-codes schema.** `CoroutineCodesPlan` keeps its identity field `body: FunctionInstanceId` and gains `frame: Option<CoroutineFramePlan>`; `None` is the request-time marker. `CoroutineFramePlan` carries `result_type`, `resume_points`, `frame_bindings: Vec<CoroutineFrameBinding>` (plan order is frame cell order), `await_result_types`, `wait_await_states`, `until_await_states`, `resources: Vec<CoroutineResourceSlot>` (effect-row order), `captures: Vec<CheckedType>`, and `capture_finalizer: Option<PlannedArtifact>`. `CoroutineFrameBinding { symbol, value_type, unwind_drop: Option<PlannedArtifact> }`; `CoroutineResourceSlot { resource: CheckedResource, indirect }`.
- **Reactive-runner schema.** `ReactiveRunnerPlan` gains `body: ReactiveRunnerBody`: `Unexpanded` | `Reaction { callback_type, resources: Vec<RunnerResourceSlot> }` | `Until { predicate_type }` | `Derived { evaluator_type, output_type }`; `RunnerResourceSlot { resource, indirect }`. The owners and sites are unchanged, so no key is re-keyed.
- **Callees and markers.** `visit_callees`/`visit_callees_mut` for `CoroutineCodes` emit the capture finalizer (when present) then each frame binding's `unwind_drop` in plan order; the three runner families name no callee. `supports_planned_callees` now reports `CoroutineCodes`; runners stay request-based. `is_expanded` is `frame.is_some()` for a pair and `body != Unexpanded` for a runner. `ProductionHooks` keeps its placeholder expander arms, so the closure stays a no-op over these families until Step 2/3.
- **Use sites.** Four `ArtifactUseSite` variants: `CoroCreation(LoweredCoroId)`, `ReactiveCallbackEnvironment(LoweredReactiveCallbackId)`, `DerivedEvaluatorEnvironment(LoweredReactiveOperationId)`, and `ReactiveRunner(LoweredReactiveOperationId)`. `check_use_site` resolves each ID in the owning body's own arenas (initializer sites index the program arenas) through new `LoweredInstanceBody::{coro, reactive_operation, reactive_callback}` accessors; `closure_stage_4_5_use_sites_outside_the_owner_arenas_are_diagnosed` proves all four arms report an out-of-arena ID. `graph_validation`'s `use_site_name` maps the variants to `coro-creation`, `reactive-callback-environment`, `derived-evaluator-environment`, and `reactive-runner` for coverage snapshots.
- **Gap fixtures (ignored, failing today).** `stage_4_5_gap_reactive_callback_environment_finalizer_is_missing` lowers a `reaction` whose callback thunk captures a droppable `CString`; legacy installs a closure-environment finalizer for it, and the test requires an interned `GcFinalizer::ClosureEnvironment` plan. `stage_4_5_gap_coroutine_body_ownership_is_compared` lowers a `coro` body with a droppable frame cell; the test compares the body thunk instance's `owned_bindings` in order and storage kind against the legacy `resume` registrations. Both are `#[ignore]`d with explicit reasons until Steps 3 and 4; running them with `--ignored` fails at the finalizer-plan lookup and at the owned-registration comparison respectively.
- **Gate:** met. Placeholder expanders are still registered, the suite passes (298 lib tests, 2 ignored; 487 + 108 integration tests), and the new corruption test diagnoses each new use-site arm.

### Step 2: Coroutine codes

- Implement `expand_coroutine_codes` and the `CoroCreation` scanner arm, including the initializer resolution and the gap-3 diagnostic in both places.
- Fixtures:
  - a `coro` with no captures, and with droppable, `Copy`, and borrowed captures;
  - frame bindings with droppable and `Copy` types;
  - zero and several resume points with mixed await result types;
  - `Wait` and `until` await states;
  - deferred resources by value and indirect (`mutable`, non-`Copy`);
  - a `coro` in an initializer and in an instance;
  - nested `coro` inside a coroutine body;
  - a generic function containing `coro` instantiated at two types, which must produce **two** pair keys (the legacy syntax key cannot express this).
- **Gate:** every `CoroutineCodes` plan is expanded with bound callees, the plan's resource pass modes agree with `is_copy_in_function` on each fixture, and closure converges. Record round and growth maxima.

**Step 2 notes (complete):**

- **Module.** New private `lower/coroutine_artifacts.rs` owns `scan_initializer`, `scan_instance`, and `expand_coroutine_codes`. `ProductionHooks::scan_initializer`/`scan_instance` compose the 4.4 cleanup scan and then the 4.5 scan, `expand` dispatches the `CoroutineCodes` arm to the expander, and `expands_body` now includes the family, so validation rejects any pair left a request-time marker.
- **Family-neutral walker.** `CleanupVisitor`/`CleanupWalker` became `LoweredOwnerVisitor`/`LoweredWalker`: every hook has a default no-op body, and the walker gained the `coro_creation(LoweredCoroId, origin)` hook (the `Coro` expression arm now calls it). `OwnerArenas`, `walk_owner`, `WalkResult`, `OwnedBindingDraft`, and `request_drop_glue` are `pub(super)` so sibling scanners reuse the traversal and the drop-glue builder instead of copying either.
- **Gap 3.** The 4.4 scanner's initializer closure resolution no longer returns silently when a resolved key is not interned; it reports "initializer closure instance was never interned". The coroutine initializer path reports the same shape of diagnostic.
- **Source of truth.** `expand_coroutine_codes` reads the body instance's local plan at `body.plan_template` (never the template plan the `LoweredCoro` indexes and never `TypedModule`), its concrete `captures()`, and the concrete frame-binding types through the new `LoweredInstanceBody::binding_symbol_type` (binding item or binding pattern). A body instance that is not a coroutine body thunk, a missing local plan, and a frame binding with no concrete type are diagnostics.
- **Planned callees.** The capture-environment finalizer is requested exactly when the thunk has any captures (legacy's gate; not the closure install gate), reusing the 4.4 `GcFinalizerKey::ClosureEnvironment` key and plan builder, so a legitimately requested finalizer may drop nothing. Frame bindings request `request_drop_glue` in plan order, which becomes frame cell order; legacy's `HashMap` order is compared as a set by the transition test.
- **Resource pass modes.** Each slot records `indirect = resource.mutable || !concrete_is_copy(value_type)` over the substituted plan row. The fixture sweep asserts every planned slot agrees with `TypedModule::is_copy_in_function(.., None)`, the legacy predicate.
- **Borrowed captures are unreachable.** The ownership checker rejects a coroutine body capturing a borrowed view ("a coroutine cannot capture a borrowed view"), so the plan's borrowed-capture fixture cannot be expressed. The fixture covers the reachable gate-fires/body-skips cases instead: a droppable owned capture (finalizer drops it), a `Copy` capture (finalizer installed, drops empty), and a mutable-storage cell capture (gate fires, body skips).
- **Fixtures.** `coroutine_codes_plans_mirror_their_body_instances` (no captures, droppable and `Copy` frame bindings, all three finalizer cases), `coroutine_codes_record_resume_states_and_await_types` (two resume points with mixed awaited types, a `Wait` state, an `until` state), `coroutine_codes_record_resource_pass_modes` (Copy value slot, mutable indirect slot, non-`Copy` indirect slot, plus the pass-mode agreement sweep), `coroutine_creation_sites_record_uses_in_initializers_and_instances` (an initializer creation and an instance creation both record a matching `CoroCreation` use), `nested_and_generic_creations_get_distinct_pairs` (a nested `coro` plus two generic instantiations give four distinct body-keyed pairs), and `expanded_pairs_bind_every_planned_callee` (the finalizer and frame-binding drops are bound to expanded plans).
- **Measurements.** All six coroutine fixtures close in 1 round; maximum growth 14 (the frame-facts fixture); the generic/creation fixtures grow at most 10.
- **Gate:** met. Every `CoroutineCodes` plan in every fixture is expanded with bound callees, the pass-mode sweep agrees with `is_copy_in_function`, and the full workspace suite passes (1246 tests, 2 ignored).

### Step 3: Reactive runners and callback environments

- Implement `expand_reactive_runner` and the `ReactiveRunner`, `ReactiveCallbackEnvironment`, and `DerivedEvaluatorEnvironment` scanner arms.
- Fixtures:
  - a `reaction` with no resources, and with value and indirect resources;
  - an explicit (non-thunk) callback;
  - `until` inside a coroutine;
  - derived bindings in an initializer and in an instance;
  - callbacks and evaluators capturing droppable, borrowed, and derived values;
  - a generic function containing `reaction`, `until`, and `derived` instantiated at two types, which must produce two keys each.
- Un-ignore the gap-1 test.
- **Gate:** every runner plan is expanded, every reactive operation that legacy gives a runner has exactly one runner use, and every installed callback finalizer has a plan.

**Step 3 notes (complete):**

- **Expander.** `expand_reactive_runner` fills all three families from the owner's lowered records: `Reaction` records the callback's concrete closure type and an ordered `RunnerResourceSlot` per callback effect resource (`indirect = mutable || !concrete_is_copy`); `Until` records the predicate type and diagnoses a non-`Bool` result (matched by the trailing `True` name component because module-scope names are qualified); `Derived` resolves the evaluator instance, records its concrete signature, diagnoses a resource-capturing evaluator, and takes the output type from the evaluator's result. A runner site must resolve to a matching operation kind in its owner, and an artifact-owned runner is a diagnostic. `ProductionHooks` dispatches the three arms and `expands_body` reports them, so no marker survives validation.
- **Scanner.** The 4.5 visitor gained the `reactive_operation` hook with a request for every reaction/`until`/derived operation: the thunk callback environment first (`ReactiveCallbackEnvironment`, gated exactly like the 4.4 closure scanner: `!requires_initialization_state && !borrowed && concrete_needs_drop`), then the runner (`ReactionRunner`/`UntilRunner`/`DerivedRunner`) with the `ReactiveRunner` use site keyed by the owning operation. Batch requests only its callback environment; signal operations, snapshot, and scope request nothing. Thunk instances resolve from the owner's `ReactiveCallback`/`DerivedEvaluator` bindings in instance owners and with the Stage 3.3 recipe in initializer owners (gap-3 diagnostic preserved).
- **Walker (`await` operands).** The family-neutral walker now descends into `await` operands through a new `await_record` accessor. Step 2's fixtures already recorded creations reachable elsewhere, but an operation or creation that is only an `await` operand (for example `await (until { .. })` or `await (coro { .. })`) was previously unreachable; the fix also lets cleanup scanning see the operand's sites. This was the one real scanner defect found while implementing this step.
- **Borrowed reactive captures are unreachable.** A callback thunk cannot capture a borrowed view (ownership: "a coroutine cannot capture a borrowed view", the same rule as coroutine bodies), and a locally built borrowed closure cannot be passed as an argument ("a borrowed closure cannot be passed as an argument"). So the plan's "borrowed" callback/evaluator bullet is not expressible; the borrowed finalizer-gate exclusion remains covered by the 4.4 local-closure fixtures.
- **Fixtures.** `reaction_runners_record_callback_types_and_resource_slots` covers a resource-free reaction, a `Copy` value resource, a mutable indirect resource, an explicit (non-thunk) callback, and a droppable moved capture whose installed finalizer lists the concrete captures; it asserts one plan per reaction and only the droppable callback installs an environment. `until_and_derived_runners_record_their_call_shapes` covers an `until` inside a coroutine body (predicate result `Bool`) plus derived bindings in the initializer and the instance, including a derived evaluator capturing another derived value (both a state-reading and a pure evaluator). `runner_sites_match_reactive_operations_and_environments` sweeps every owner: one runner use per runner-bearing operation, each use's family matching its operation kind, and every environment use naming a `GcFinalizer::ClosureEnvironment` plan. `generic_reactive_sites_get_one_runner_per_instantiation` instantiates a generic reaction, `until`, and derived binding at `I32` and `U8`, proving two distinct instance-keyed runners per family.
- **Measurements.** All reactive fixtures close in 1 round; maximum growth 13 (the generic reactive fixture).
- **Gate:** met. Every runner plan is expanded, every runner-bearing operation has exactly one runner use whose family matches (swept), every installed callback environment has a closure-finalizer plan, and the full workspace suite passes (1251 tests, 1 ignored: the Step 4 coroutine-ownership gap).

### Step 4: Coroutine-body ownership

- Extend the recorder attribution for `resume` and add the collector's frame-binding exclusion.
- Resolve the completed-body frame-binding question with a fixture, and record the outcome.
- Un-ignore the gap-2 test.
- **Gate:** per emitted function, including every coroutine body, legacy's owned registrations match `owned_bindings` in order and storage kind.

**Step 4 notes (complete):**

- **Recorder attribution.** `ModuleEmitter` gained a test-only `legacy_owned_function`; `legacy_record_owned` now takes `environment.function_id.or(self.legacy_owned_function)`, so a nested emission's own id always wins. Around state 0, `ensure_coroutine_codes` sets the legacy function key to the body thunk with its concrete type (the template signature substituted through the active substitutions, its effect row replaced by the plan's deferred effects) and sets `legacy_owned_function`, restoring both after. Production emission reads neither key and `environment.function_id` stays `None` in `resume`, so `is_copy_in_function(.., None)` and every other production decision are unchanged.
- **Collector exclusion.** `LoweredWalker::register_binding` returns early for a symbol in the owning instance's local plan `frame_bindings`. This applies to the 4.4 scanner and the collector alike, so a frame cell gets no `OwnedBinding` use and no owned record; its glue lives in the pair plan's `unwind_drop`.
- **Completed-body leak (confirmed and mirrored).** Every body-local `let` and pattern binding inside a `coro` body is a frame binding (`coroutine_lower::scan_item` recurses through blocks, match arms, loops, and `with` bodies), so `ensure_coroutine_codes` pre-seeds every one as a frame cell; `allocate_binding_cell` returns early and `track_symbol_ownership` never registers it. A completed body's normal return therefore runs `drop_all_owned` over an empty owned set and **never drops its droppable frame bindings**; the only drop is the cancel unwind's conditional cell drop (the pair plan's `unwind_drop`). The leak is recorded as a latent defect and mirrored, not fixed, by Stage 4.5.
- **Fixture.** `stage_4_5_coroutine_body_ownership_matches_legacy` (un-ignored) lowers a `coro` body with a `CString` frame cell and asserts: the body thunk instance's `owned_bindings` is empty and equals the recorder's registrations for that instance in order and storage kind; the pair plan carries the frame binding's bound `unwind_drop`; and legacy emits the CString unwind drop. Every emitted function, coroutine bodies included, now flows through the same per-instance comparator in the 4.4 transition test.
- **Gate:** met. The coroutine-body registrations match `owned_bindings` (both empty for the reachable shape, which is the parity the recorder proves), the leak is documented and mirrored, and the full workspace suite passes 1252 tests with no ignored test remaining.

### Step 5: Validation

- `validate_artifact_closure` rejects:
  - a `CoroutineCodes` plan whose body instance is not a coroutine body thunk;
  - a plan whose frame facts disagree with the body's local plan;
  - a `coro` site whose use names a different body instance than its binding;
  - a runner whose site does not resolve to a matching reactive operation kind in its owner.
- Add a corruption test for each.
- **Gate:** the validators pass over the standard library and every 4.5 fixture.

**Step 5 notes (complete):**

- **Re-expansion validator.** `check_stage_4_5` runs from `validate_artifact_closure` after the owned-binding check. It re-expands every expanded `CoroutineCodes` plan from its body instance and requires the rebuilt plan to equal the stored one modulo bound callee ids, so an edited frame fact (result/await types, resume points, `Wait`/`until` states, binding order and types, resource order/pass modes, captures, finalizer presence) is rejected. The expander's own diagnostics cover the structural cases: a body instance that is not a coroutine body thunk, a missing body or local plan, and a frame binding with no concrete type.
- **Runner plans.** Every expanded runner re-expands from the owner's operation and callback records and must equal the stored plan; the expander rejects a missing owner or record, a site that is not a callback/operation of the right kind, a callback that no matching operation references, an owner with no matching operation kind, an `until` predicate whose result is not `Bool`, and a derived evaluator that captures resources.
- **Creation uses.** For every instance body, each `CoroCreation` use's artifact key must name the body instance its `LoweredBindingSite::Coro` binding resolves to; a missing binding is a diagnostic. Initializer uses re-resolve their thunk with the Stage 3.3 recipe through the new shared `initializer_body_instance` helper (also used by the scanner) and compare. So a use can never name a body instance other than the one Stage 5 would emit from that site.
- **Corruption tests.** `a_coroutine_pair_over_a_non_thunk_body_is_diagnosed`, `a_coroutine_pair_disagreeing_with_its_local_plan_is_diagnosed`, `a_coro_creation_use_naming_another_body_is_diagnosed`, and `a_runner_site_without_a_matching_operation_is_diagnosed` mutate a closed program and require the specific diagnostic.
- **Gate:** met. Production lowering validates every fixture and the standard library through `check_stage_4_5`, and the full workspace suite passes 1256 tests.

### Step 6: Legacy transition comparison

Extend the `#[cfg(test)]` recorder in `codegen.rs`.

- **Record** each `ensure_coroutine_codes` creation:
  - the body `SyntaxId` and the active substitutions at creation;
  - the substituted frame-binding types in plan order, the result type, and the await result types;
  - `resume_points` and the `Wait`/`until` states;
  - each bundle resource's pass mode;
  - whether the capture finalizer is called;
  - the set of frame bindings dropped on unwind.
- **Record** each *request* for a pair (every `compile_coro_expression`), so the test can show where the syntax-keyed cache aliased two instantiations.
- **Record** each runner creation: its family, legacy key (call `SyntaxId` or evaluator `FunctionId`), callback type, and resource pass modes. For `until`, also record whether the name was reused.

The transition test then requires:

- every legacy pair matches exactly one `CoroutineCodes` plan on all recorded facts, with unwind drops compared as a set;
- every plan whose template matches a legacy pair either matches it or is explained as an aliased instantiation (same body syntax, different substitutions). The test asserts the aliasing explicitly rather than skipping it;
- every legacy runner creation matches exactly one runner plan by owner and site, and vice versa, with `until` reuse explained the same way;
- the finalizer comparison from 4.4 now includes coroutine thunk environments and reactive callback environments, and has no exclusions.

Fixtures must be non-vacuous. Assert coverage of all three runner families, a pair with and without a capture finalizer, a pair with unwind drops, and every new use-site kind. Extend `stage_4_4_cleanup_matches_legacy_on_standard_library_values`, or add a sibling test, for a program that uses the standard library's task, scheduler, `Wait`, and reactive surfaces.

**Gate:** the transition test passes on the representative fixtures and on the standard-library program.

### Step 7: Final gates and handoff

- Run `cargo fmt --all -- --check`, `cargo check --workspace`, `cargo test --workspace --quiet` (report the total), `git diff --check`, and the CLI `--emit llvm`, `--emit object`, and `run` paths with the worktree standard library on a program that creates, awaits, cancels, and drops coroutines and uses `reaction`, `until`, and `derived`.
- Record the final schemas, the use-site table, the observed maxima, the latent defects that were mirrored, and the Stage 4.6 and Stage 5 hand-offs:
  - **Stage 4.6:** the runtime surfaces each pair and runner body implies.
  - **Stage 5:** emit the pair from the body instance plus `CoroutineFramePlan`, and emit unwind drops in plan order. Name runners and pairs by catalog ordinal, not by `SyntaxId`.
- Update [STAGE_4_GENERATED_ARTIFACTS_BREAKDOWN.md](STAGE_4_GENERATED_ARTIFACTS_BREAKDOWN.md) and [TYPED_LOWERING_PLAN.md](TYPED_LOWERING_PLAN.md).

## Risks

- **Ownership parity inside coroutine bodies** is the highest risk. It is new ground: no comparison has ever covered it (gap 2). Step 4 is the detector, and any mismatch is fixed in the collector, not in the backend.
- **Legacy aliasing makes legacy an imperfect reference.** For a generic `coro`/`until`, legacy emits one body under the first instantiation's substitutions. The comparison must match each legacy pair to exactly one plan and *explain* the rest, never loosen the match to types.
- **Non-deterministic legacy unwind order.** Frame-binding unwind drops follow `HashMap` order in legacy. Compare them as a set and never assert order against legacy.
- **Pass-mode drift.** `is_copy_in_function(.., None)` versus `concrete_is_copy` could disagree for a type whose `Copy` evidence depends on bounds. Step 2 asserts agreement. If they disagree, record the legacy predicate's decision rather than the owned one.
- **Scanner ordinals.** The 4.5 scanner and the gap-1 finalizers append new artifacts after each owner's 4.4 requests. Programs with droppable reactive captures gain finalizer artifacts. Stage 3, 4.3, and 4.4 prefix ordinals must be unchanged for programs without them; treat every other snapshot diff as review-required.

## Stage boundary

- Stage 4.5 delivers expanded coroutine-codes and reactive-runner plans, every `coro` and reactive-operation site use, the reactive callback environment finalizers, coroutine-body ownership parity, and the legacy transition comparison for these families. After it, the catalog is closed for every family except `ExternAdapter` and the runtime requirement set (Stage 4.6).
- Stage 4.5 does not change emission, fix the mirrored latent defects, record runtime requirements (Stage 4.6), or make LLVM consume the catalog (Stage 5).
