# Stage 4.3 Plan: Structural Methods, Constructor Adapters, and Formatting

**Status:** In progress. Step 1 (plan schema, planned callees, and binding pass) is complete. Stage 4.2 is complete through `5e7555e`: the fixed-point closure engine (`lower/artifact_closure.rs`), `ArtifactFamilyHooks` with placeholder `ProductionHooks`, closure use/edge storage, and `validate_artifact_closure` are in place. Stage 4.3 is the first substage to register real expanders.

## Goal and boundary

Stage 4.3 replaces the Stage 4.1 placeholder plans for two artifact families with complete owned plans:

- `ConstructorAdapter`: a constructor used as a callable value.
- `StructuralMethod`: the seven `StructuralTraitMethod` kinds, which are `Debug` (product and sum), `Index`, `MutateIndex`, `DerefIndex`, `DerefMutateIndex`, `IntoIterator`, and `Iterator` (`next`).

It also closes the formatting dependencies that these bodies and string templates need.

After Stage 4.3, each plan records every decision the legacy backend currently makes while emitting these bodies (`ensure_constructor_adapter`, `structural_trait_method_code`, and the `compile_structural_*_body` helpers in `codegen.rs`). The plan also names every callee as a catalog identity. Stage 5 must be able to emit these bodies from the plan alone. It must not need `TypedModule::{trait_impl_method, structural_trait_method, instantiated_trait_method_type, is_copy_in_function, type_needs_drop}`, `resolved().standard_trait(..)`, `standard_function_name_matches`, or the representation matching in `compile_structural_next_body`.

Out of scope:

- Drop glue and finalizer *plans* belong to Stage 4.4. Stage 4.3 only *requests* their keys where its bodies need them, and the Stage 4.1 placeholder plan is enough for that.
- Coroutines, reactive runners, extern adapters, and runtime requirements belong to later substages.
- Stage 4.3 makes no backend change and no ABI change. Legacy emission remains the reference.

Stage 4.3 adds **expanders only**. Constructor-adapter and structural-method artifacts are already requested by the Stage 3.3 worklist, and Stage 3.4 already binds their use sites (`LoweredBoundTarget::Artifact`). String-template helper sites are already bound (`FormattingConstructor`/`FormattingWrite`/`FormattingFinish`, `Interpolation`). No 4.3 scanner is needed. The 4.3 scanner slot in `ProductionHooks` stays empty. Step 1 verifies this claim.

Line references are against `5e7555e` and will drift; function names are authoritative.

## Legacy behavior to capture

| Legacy function | Behavior the plan must record | Hidden dependency |
| --- | --- | --- |
| `ensure_constructor_adapter` | Closure ABI function over `function_type`. It builds the result product from the flattened parameters (skipping the environment). When the result is `Ref payload`, it wraps the product in a GC allocation (`build_ref_value`). | `build_ref_value` sets a payload GC finalizer when `type_needs_drop(payload)` |
| `structural_trait_method_code` prologue | Loads each indirect, non-mutated parameter from its pointer before running the body. | Parameter masks come from `function_type`. This is ABI and stays in the backend. |
| `compile_structural_debug_body` (product) | Writes `"("`. For each element, it writes `", "` before every element except the first, then `name` and `": "` for named elements, then calls `Debug.fmt` on the element. Finally it writes `")"`. | `standard_trait("Debug")` plus the first trait method, selected through `trait_method_code` for each element type. `Formatter.write` is found by name (`formatter_write`) for every literal. |
| `compile_structural_sum_debug_body` | Switches on the tag. Each alternative calls `Debug.fmt` on its payload. No literals are written. | Per-alternative `trait_method_code` |
| `compile_structural_index_body` | Heterogeneous product: bounds trap, switch per element, coerce element → output, merge. Homogeneous product: `compile_index_load` over a stack copy. | none beyond coercion |
| `compile_structural_mutate_body` / `compile_structural_replace` | Bounds trap, element slot, drop the old element if `type_needs_drop(element)`, then store. | `compile_drop_value(element)` |
| `compile_structural_deref_index_body` | If the payload is a non-variadic homogeneous product with a `Copy` element (`is_copy_in_function(element, None)`), it loads the element directly through the reference. Otherwise it loads the payload and delegates to `Index` for `(payload, position, output)`. | `standard_trait_id("Index")`, `trait_method_id`, `build_trait_method_call` → `instantiated_trait_method_type` + `trait_method_code` |
| `compile_structural_deref_mutate_body` | Loads the reference, then delegates to `MutateIndex` for `(payload, position, element)` and passes the payload address as `mut T`. | `standard_trait_id("MutateIndex")` + the same delegated call path |
| `compile_structural_into_iterator_body` | Rebuilds the source product from its flattened values and pairs it with cursor `0`. | none |
| `compile_structural_next_body` | Returns `Done` when the cursor is out of range, and otherwise switches on the cursor and yields `(item, (product, cursor + 1))`. It finds `Done`/`Yield` among the result sum's alternatives by comparing `Distinct.representation` with `iter` and `(item, iter)`, then coerces into the result sum. | representation matching on checked types |
| `compile_string_template` | `formatter_new`, then the parts (literals through `formatter_write`, interpolations through the selected `Display`/`Debug` method), then `formatter_finish`. | Already recorded in Stage 3/4.1. Only verification is left (Step 5). |

## Design

### Plan types (`lower/artifact_plan.rs`)

Replace the placeholder structs with complete plans. Keep every Stage 4.1 identity field so that `matches_key` still rebuilds the key. Extend `matches_key` so it checks the new identity-relevant fields that the key contains (for `StructuralMethod`: trait, method, arguments, and callable type, not just the structural kind).

```rust
pub(crate) struct ConstructorAdapterPlan {
    pub symbol: SymbolId,
    pub type_id: TypeId,
    pub adapter: LoweredCallableAdapter,
    pub callable_type: CheckedFunctionType,   // concrete; the adapter's ABI
    pub parameters: Vec<CheckedType>,         // flattened parameter types, in slot order
    pub product: CheckedType,                 // the product built from `parameters`
    pub construction: ConstructorConstruction,
}
pub(crate) enum ConstructorConstruction {
    /// Return the product value unchanged (ordinary nominal wrapping).
    Value,
    /// GC-allocate the product as a `Ref` payload.
    ManagedRef { payload: CheckedType, finalizer: Option<PlannedArtifact> },
}

pub(crate) struct StructuralMethodPlan {
    pub structural: StructuralTraitMethod,
    pub trait_id: TraitId,
    pub method: TraitMethodId,
    pub arguments: Vec<CheckedType>,          // completed, concrete
    pub callable_type: CheckedFunctionType,   // concrete method type; the ABI
    pub body: StructuralBody,
}
pub(crate) enum StructuralBody {
    ProductDebug { steps: Vec<DebugStep>, write: PlannedInstance },
    SumDebug { alternatives: Vec<DebugDelegate> },  // in tag order
    IndexSwitch { elements: Vec<IndexedElement>, output: CheckedType },           // heterogeneous
    IndexLoad { element: CheckedType, length: usize, output: CheckedType },       // homogeneous
    MutateReplace { element: CheckedType, length: usize, drop_previous: Option<PlannedArtifact> },
    DerefIndexLoad { element: CheckedType, length: usize, output: CheckedType },  // Copy fast path
    DerefDelegate { payload: CheckedType, delegate: TraitDelegate },              // Index or MutateIndex
    IntoIterator { source: CheckedType, iterator: CheckedType },
    Next { product: CheckedType, iterator: CheckedType, item: CheckedType,
           elements: Vec<IndexedElement>, result: CheckedType,
           done: SumAlternative, yield_: SumAlternative },
}
pub(crate) enum DebugStep { Write(String), Element { index: usize, delegate: DebugDelegate } }
pub(crate) struct DebugDelegate { pub value_type: CheckedType, pub callee: PlannedCallee, pub callee_type: CheckedFunctionType }
pub(crate) struct TraitDelegate { pub trait_id: TraitId, pub method: TraitMethodId,
                                  pub arguments: Vec<CheckedType>, pub callee: PlannedCallee,
                                  pub callee_type: CheckedFunctionType }
pub(crate) struct IndexedElement { pub index: usize, pub element: CheckedType, pub coercion: Option<(CheckedType, CheckedType)> }
pub(crate) struct SumAlternative { pub index: usize, pub alternative: CheckedType }
```

The exact field names are open. The rules are not:

- **Concrete values only.** No declared parameters, effect variables, `Inferred`, or `Error` may appear in a plan.
- **Layout stays in the backend.** Record `CheckedType`s and element and alternative indices, never LLVM types, sizes, or alignment.
- **Record the literal strings exactly.** `"("`, `", "`, the element name, `": "`, and `")"` go into the plan in emission order, so Stage 5 reproduces byte-identical output without re-deriving punctuation.
- **Record coercions explicitly as `(from, to)` pairs.** Record nothing when the two types are equal.
- **Record each callee's concrete function type** (`callee_type`, from `instantiate_method_type` or the template signature). This lets Stage 5 compute the delegated call's argument ABI without `instantiated_trait_method_type`.

### Naming callees whose ids don't exist yet

The Stage 4.2 handoff says that "a plan built during expansion cannot see the fresh id" of an instance or artifact it requests in the same expansion. Stage 4.3 is the first family to hit this, so it adds a small generic mechanism that Stages 4.4 and 4.5 will reuse. It lives in `artifact_plan.rs`, and the engine gains one post-closure pass.

```rust
pub(crate) struct PlannedInstance { pub key: InstanceKey, pub instance: Option<FunctionInstanceId> }
pub(crate) struct PlannedArtifact { pub key: ArtifactRequestKey, pub artifact: Option<ArtifactOrdinal> }
pub(crate) enum PlannedCallee { Instance(PlannedInstance), Artifact(PlannedArtifact) }
```

- The expander fills in `key` and leaves the id as `None`. For every planned callee, it also emits the matching `ClosureRequest` (an instance request with `InstanceResolutionTarget::Root`, or an artifact request with the family's placeholder plan and kind).
- Add a new pass, `LoweredProgram::bind_artifact_plan_callees`. It runs in `close_artifact_catalog` after the loop reaches a fixed point and before names are assigned. It resolves every `PlannedInstance`/`PlannedArtifact` key through `SpecializationCatalog::{instance_ordinal, artifact_ordinal}` and writes the id back. A missing key is a diagnostic. It cannot happen if the expander emitted the matching request, so the diagnostic points to a bug in the expander.
- Plans expose `planned_callees()` through an exhaustive match. `validate_artifact_closure` uses it to check three things:
  1. Every planned callee is bound.
  2. Every planned callee has a matching artifact-owned edge, matched by target and kind. The mapping from planned callees to edges is one-to-one in plan order.
  3. No artifact-owned edge lacks a planned callee.
- The Stage 4.2 fixed-point re-check stays request-based. Also compare the freshly expanded plan with the stored plan *modulo bound ids*: add `LoweredArtifactPlan::eq_ignoring_bindings`, and treat a mismatch as a fixed-point violation. This catches expanders that are nondeterministic or that depend on the catalog state.

Rationale: expanders stay stateless, which the 4.2 contract requires, and ordinals stay first-discovery. Binding happens only once the graph is final.

### Selecting a delegated trait method

Every nested `Debug`, `Index`, and `MutateIndex` selection uses the same helper in the new 4.3 module. Call it `select_concrete_trait_method(program, origin, trait_id, method, arguments) -> Result<(PlannedCallee, CheckedFunctionType, ClosureRequest), Diagnostic>`.

1. Build `TraitEvidence::DeclaredBound { trait_id, method: Some(method), arguments, prerequisites: [] }`. Resolve it with `program.resolve_trait_evidence(origin, Some(&evidence), &SubstitutionEnvironment::default())`. Stage 3.2 already proved this resolver agrees with `trait_impl_method` (explicit implementation first, structural derivation otherwise), which is the legacy `trait_method_code` precedence. Record any divergence found in the transition test as a bug, and do not work around it.
2. Compute the concrete method type with `worklist::instantiate_method_type(program, origin, trait_id, method, &completed_arguments)`.
3. `ExplicitImplementation { function, .. }` → `ClosureRequest::Instance` for `function`, with `Root` target, the concrete method type, the instance's substitutions from `program.resolve_substitutions`/the implementation header (follow the handoff recipe), and the resolved evidence. The result is `PlannedCallee::Instance`, with `LoweredInstanceDependencyKind::TraitMethod`.
4. `Structural { structural, arguments, .. }` → `StructuralMethodKey::new(...)` → `ClosureRequest::Artifact` with a placeholder `StructuralMethodPlan` and `LoweredArtifactDependencyKind::StructuralMethod`. The result is `PlannedCallee::Artifact`.
5. `RejectedImplementation`, or no match → a diagnostic at the artifact origin. This is a checker/lowering disagreement, because the checker accepted the structural obligation.

Trait and method identities come from `program.semantic_ids.{debug_trait, index_trait, mutate_index_trait}` and the owned trait catalog's declared method order (the legacy code uses `methods.first()`), never from names. A missing semantic id is a diagnostic.

### Cross-family requests

- **Constructor `ManagedRef`** whose payload needs drop (`program.concrete_needs_drop(payload)`): request `ArtifactRequestKey::GcFinalizer(GcFinalizerKey::Payload(..))` with the 4.1 placeholder `GcFinalizerPlan::Payload` and `LoweredArtifactDependencyKind::GcFinalizer`. Stage 4.4 fills in the finalizer's plan.
- **`MutateReplace`** whose element needs drop: request `ArtifactRequestKey::DropGlue(element)` with the placeholder `DropGluePlan` and kind `DropGlue`. Whether Stage 5 calls drop glue or inlines it is Stage 4.4's decision. Stage 4.3 only guarantees the key is present and planned.
- **`ProductDebug` `Formatter.write`**: request an instance of `program.string_formatting.write` (`Root`, template signature, no substitutions) with kind `FormattingWrite`. If `write` is `None` while a product Debug artifact exists, raise a diagnostic.

These requests use Stage 4.4 key families before 4.4 lands. That works because the 4.1 placeholders already satisfy `matches_key`, and the 4.2 engine expands them with the placeholder arm. Stage 4.4 must not rename or re-key them.

### Module layout

- New private child module `lower/structural_artifacts.rs`. It holds `expand_constructor_adapter`, `expand_structural_method`, the per-kind body builders, and `select_concrete_trait_method`.
- `artifact_plan.rs` holds the plan types, `planned_callees`, `eq_ignoring_bindings`, and the extended `matches_key`.
- `ProductionHooks::expand` calls the two expanders from its `ConstructorAdapter` and `StructuralMethod` arms. The other arms keep their placeholders.

## Implementation sequence

### Step 1: Plan schema, planned callees, and binding pass

- Define the plan types above, `PlannedInstance`/`PlannedArtifact`/`PlannedCallee`, `planned_callees()`, `eq_ignoring_bindings`, and the stricter `matches_key`. The Stage 3.3 worklist still creates placeholder-shaped plans at request time. Make the request-time plan a minimal "unexpanded" form (for example `StructuralBody::Unexpanded` and a `ConstructorConstruction` computed later). Otherwise the worklist has to compute full bodies, and body building belongs to the expander. Validation must reject an `Unexpanded` body after closure.
- Add `bind_artifact_plan_callees` to the closure engine after the fixed point. Extend `validate_artifact_closure` with the planned-callee checks and the plan-equality re-check. Extend the normalized snapshot in `lower.rs` tests so it renders plan bodies and bound callees.
- Verify the "no 4.3 scanner" claim. Check that every constructor-value and structural site in materialized instance bodies is already bound to its artifact (`LoweredBoundTarget::Artifact`). Check that initializer-owned constructor and structural requests exist as Stage 3 request roots. Record the result in the step notes. If a site is not bound, add a 4.3 scanner arm and a use-site variant, following the 4.2 recipe.
- **Gate:** with placeholder expanders still in place, every existing test passes, and the snapshot shows `Unexpanded` bodies. A synthetic test (reuse the 4.2 `TestHooks`) proves three things: a plan that names a freshly requested instance and a freshly requested artifact gets both ids bound after closure; an unbound or missing callee is diagnosed; and a plan/edge mismatch is diagnosed.

**Step 1 notes (complete):**

- **Schema.** `ConstructorAdapterPlan` carries `symbol`, `type_id`, `adapter`, `callable_type`, and `construction: ConstructorConstruction` (`Unexpanded` | `Value { parameters, product }` | `ManagedRef { parameters, product, payload, finalizer }`). `StructuralMethodPlan` carries `structural`, `trait_id`, `method`, `arguments`, `callable_type`, and `body: StructuralBody` (the ten variants from the design plus `Unexpanded`). `matches_key` now rebuilds the whole key: symbol/type-id/adapter and callable type for adapters, and trait/method/arguments/callable type as well as the structural kind for structural methods.
- **Planned callees.** `PlannedCallee { Instance(PlannedInstance) | Artifact(PlannedArtifact) }`; each names a key, the id left `None` until binding, and the dependency kind its edge records. Plans expose `planned_callees()`/`planned_callees_mut()` as `PlannedCalleeRef`/`PlannedCalleeRefMut` views in request order, plus `eq_ignoring_bindings` (clone, clear bindings, compare) and `is_expanded`/`supports_planned_callees`.
- **Binding pass.** `LoweredProgram::bind_artifact_plan_callees` runs at the fixed point before names are assigned, resolving every planned key through `SpecializationCatalog::{instance_ordinal, artifact_ordinal}` and diagnosing a key the expander failed to request. `Arena::iter_mut` was added for the walk.
- **Validation.** `check_planned_callees` requires every callee to be bound to the key it names and matched one-to-one, in plan order, with the artifact's own instance/artifact edges by target and kind, and rejects an artifact-owned edge with no planned callee. It runs only for plan schemas that carry callee slots (`ConstructorAdapter`/`StructuralMethod`); Stage 4.4-4.6 families keep their raw-request representation until their own plan schema lands. `check_closure_fixed_point` now also requires a re-expanded plan to equal the stored plan modulo bound ids. A new defaulted hook method `expands_body(key)` lets validation reject a request-time marker (`Unexpanded`) only for families whose expander the hook set registers; it stays false in Stage 4.2/Test hooks, so placeholder-era snapshots are unchanged.
- **No-scanner verification.** `constructor_and_structural_sites_need_no_stage_4_3_scanner` (graph_validation tests) lowers a fixture with a constructor value plus Debug/Index/MutateIndex/IntoIterator/Iterator sites and proves every constructor-value and structural-evidence binding is `LoweredBoundTarget::Artifact` and every constructor/structural artifact has a Stage 3 `Instance`/`Initializer` request root. No 4.3 scanner arm or use-site variant is needed; the 4.3 scanner slot stays empty.
- **Gate:** met. The whole workspace suite passes (268 compiler lib tests including four new binding/edge/fixed-point tests), `normalized_program_snapshot` renders plan bodies and bound callees, and every plan in the catalog still shows `Unexpanded` with `ProductionHooks`.

### Step 2: Constructor adapters

- Implement `expand_constructor_adapter`. Take the parameter slot types from the concrete callable type's flattened parameter, the same flattening `build_product_value(&parameters[1..])` observes. Take the product from those types. Classify construction as `ManagedRef` when the concrete result is `CheckedType::Ref(payload)`, and cross-check this against the Stage 2 constructor target's recursive-construction class. Request the payload finalizer if one is needed.
- Fixtures: an ordinary nominal constructor value, a `Ref` constructor value with a droppable payload and with a `Copy` payload, a constructor value in a generic instance at two types (two keys), and named and positional product parameters.
- **Gate:** every constructor adapter's plan is complete and validated. Finalizer keys appear exactly when legacy `build_ref_value` would set a finalizer. The fixtures and the workspace suite pass.

### Step 3: `Index`, `MutateIndex`, `IntoIterator`, and `Iterator`

- `Index`: choose `IndexSwitch` or `IndexLoad` using the same predicate as legacy (`product.homogeneous_element().is_none()`), with per-element coercions to the output type.
- `MutateIndex`: `MutateReplace` with `drop_previous` when `concrete_needs_drop(element)` holds.
- `IntoIterator`: record the source product and the derived iterator product `(P, USize)`.
- `Iterator.next`: record the iterator, the inner product, the item, per-element coercions to `item`, and the `Done`/`Yield` alternatives. Resolve these from owned metadata: a `Distinct` alternative whose representation equals `iter` is `Done`, and one whose representation equals `(item, iter)` is `Yield`. Keep this equality check in the expander, and diagnose if either alternative is missing or ambiguous. Stage 5 then indexes alternatives instead of matching representations.
- Fixtures:
  - heterogeneous and homogeneous product indexing, including coercion into a sum output;
  - mutation of `Copy` elements and of droppable elements;
  - `for` over a mixed product (`IntoIterator` plus `next`);
  - an iterator whose item type needs coercion;
  - each kind in two generic instances.
- **Gate:** every plan of these kinds is complete. `drop_previous` matches `type_needs_drop`. The `Done`/`Yield` indices match what legacy representation matching selects.

### Step 4: `DerefIndex`, `DerefMutateIndex`, and `Debug`

This step exercises nested selection.

- `DerefIndex`: `DerefIndexLoad` when the payload is a non-variadic homogeneous product with a `concrete_is_copy` element (mirroring `is_copy_in_function(element, None)`). Otherwise `DerefDelegate` to `Index` for `(payload, position, output)` through `select_concrete_trait_method`.
- `DerefMutateIndex`: always `DerefDelegate` to `MutateIndex` for `(payload, position, element)`.
- Product `Debug`: emit the step list in the legacy order, with one `select_concrete_trait_method(debug, [element_type])` per element and the `Formatter.write` instance.
- Sum `Debug`: one delegate per alternative in tag order, and no write.
- Convergence cases to test:
  - Debug of a product whose field is another product → nested structural artifact.
  - Debug of a product whose field type has an explicit, generic `Debug` implementation → new instance requested by the artifact, then traversed and materialized in the next round.
  - A recursive nominal type reached through `Ref` or a sum → dedups to one key, with a back-edge.
  - `DerefIndex` over `Ref` of a heterogeneous product → delegates to a structural `Index` artifact.
  - `DerefIndex` over `Ref` of a type with an explicit `Index` implementation → delegates to an instance.
- **Gate:** all structural kinds are expanded. Every delegate is bound to the same callee legacy `trait_method_code` selects (proved in Step 5). Closure converges within the 4.2 bounds. Record the observed round and growth maxima.

### Step 5: Formatting verification and legacy transition comparison

- Formatting: assert that every string template in every materialized body binds `FormattingConstructor` and `FormattingFinish`, and binds `FormattingWrite` exactly when it has a literal part. Assert that every interpolation binds its `Display`/`Debug` callee as an instance or structural artifact. Assert that every product-Debug plan binds `Formatter.write`. Structural `Debug` that is reached *only* through an interpolation must also be reachable in the catalog.
- Extend the `#[cfg(test)]` legacy recorder in `codegen.rs`. This is the start of the Stage 4.7 hook. Inside `structural_trait_method_code` and `ensure_constructor_adapter`, record:
  - each nested `trait_method_code` result, as `(structural key, callee FunctionId-or-structural kind + arguments)`;
  - each `compile_formatter_write_literal` string, in order;
  - the chosen `DerefIndex` path;
  - the `next` `Done`/`Yield` alternatives;
  - whether a finalizer was set.
- A transition test runs the representative fixtures through both legacy emission and lowering. It requires every legacy constructor and structural body to match exactly one artifact plan: the same literal sequence, the same delegated callees (instance template plus substitutions, or structural key), the same fast-path choice, the same alternatives, and the same finalizer presence. Legacy bodies that are never emitted (for example unreachable structural keys) must not appear in the catalog either, and vice versa.
- **Gate:** the transition test passes on non-vacuous fixtures covering every structural kind and constructor shape.

### Step 6: Gates and handoff

- Run `cargo fmt --all -- --check`, `cargo check --workspace`, `cargo test --workspace --quiet`, and `git diff --check`. Run the CLI `--emit llvm`, `--emit object`, and `run` paths with the worktree standard library.
- Record in this file:
  - the final plan schemas;
  - the planned-callee mechanism and its validator (for Stages 4.4 and 4.5 to reuse);
  - the cross-family keys 4.3 now requests (`GcFinalizer::Payload` and `DropGlue`), which 4.4 must fill without re-keying;
  - the observed closure maxima.
- Update [STAGE_4_GENERATED_ARTIFACTS_BREAKDOWN.md](STAGE_4_GENERATED_ARTIFACTS_BREAKDOWN.md) and [TYPED_LOWERING_PLAN.md](TYPED_LOWERING_PLAN.md).

## Risks

- **Selection precedence drift.** If `resolve_trait_evidence` and legacy `trait_method_code` disagree for a concrete type (for example a conditional implementation versus structural derivation), the Step 5 transition test is the detector. Fix the resolver, because it is shared with Stage 3. Do not special-case the expander.
- **Flattening assumptions.** Legacy structural bodies receive top-level products flattened into separate parameters (for example `next` receives `(P, USize)` as two values). The plan records logical types, and Stage 5 re-derives flattening from `callable_type`. Keep flattening out of the plan. Document the observed shape in the Step 3 notes so Stage 5 doesn't have to rediscover it.
- **Copy predicate mismatch.** The `DerefIndex` fast path depends on `concrete_is_copy` agreeing with `is_copy_in_function(element, None)`. Add a direct agreement assertion to the Step 4 fixtures.
- **Ordinal churn.** Expanding structural Debug now appends nested artifacts and instances, which is expected. Existing snapshot tests that assert artifact counts or ordinals will change. Treat each diff as review-required, and confirm that Stage 3 prefix ordinals are unchanged.

## Stage boundary

- Stage 4.3 delivers complete constructor-adapter and structural-method plans, the planned-callee binding mechanism, and formatting-closure verification. The artifact catalog is closed for these families relative to legacy emission.
- Stage 4.3 does not fill drop-glue or finalizer plans (Stage 4.4), coroutine or reactive plans (Stage 4.5), or extern adapters and runtime requirements (Stage 4.6). It makes no change to backend emission.
