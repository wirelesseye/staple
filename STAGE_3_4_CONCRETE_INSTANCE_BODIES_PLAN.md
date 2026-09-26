# Stage 3.4 Plan: Materialize Concrete Instance Bodies

**Status:** Planned. Stage 3.3 is implemented through `f30ee2b`; Stage 3.4 has not been implemented.

## Goal and boundary

Give every reachable `LoweredFunctionInstance` an owned, concrete body that can be traversed without reopening its generic `LoweredFunction` template or asking `TypedModule` for type arguments, call targets, or trait selection. Preserve the Stage 3.3 instance and artifact ordinals, names, root order, dependency edges, and source evaluation order. The existing LLVM backend remains in use during Stage 3; Stage 5 switches emission to the new graph.

Stage 3.3 already interns each instance before visiting its body and stores its `FunctionId`, pruned `SubstitutionEnvironment`, relevant parameters, selected evidence, and typed dependency/artifact edges in `lower/worklist.rs`. Stage 3.2 provides concrete substitution and trait-evidence resolution. Stage 3.4 consumes those results; it must not repeat checker inference or add a second specialization queue. Stage 4 owns generated constructor, structural, formatting, cleanup, and coroutine helper bodies and any dependencies found inside them.

## Completion gate

- Each instance has one immutable, instance-owned body with concrete signature, bounds used by the body, parameter/capture metadata, checked types and effects, and local node references. Two instances of the same template share no mutable body nodes. Template arenas remain intact for diagnostics and transition comparison.
- Every known source-function call, callable value, implicit thunk, reactive evaluator/callback, and coroutine body link in an instance points to the existing `FunctionInstanceId`. Trait sites point to the selected explicit instance or the existing typed structural artifact. Indirect calls, externs, intrinsics, ordinary constructor calls, and unresolved Stage 4 compiler helpers retain their distinct routes.
- All relevant type/effect parameters and checked placeholders are absent from emitted instance metadata. Recursive nominal representations remain compact references by semantic `TypeId` and concrete arguments.
- Concrete-sensitive ownership, drop, resource, aggregate-shape, and coroutine-frame facts agree with checked rules. Evaluation steps, ABI slot order, mutation/move decisions, and control-flow structure are preserved.
- A validator can walk each instance body and confirm that every local ID and target reference exists, every required dependency matches its Stage 3.3 edge or artifact request, and no body lookup needs a generic template or `TypedModule` side table. Focused fixtures and the full workspace gate pass.

## Representation and ownership contract

Add an instance-body record under `LoweredFunctionInstance` (or an arena indexed by `FunctionInstanceId`) with its own typed arenas for blocks, items, expressions, patterns, places, calls, callable values, resource providers/uses, `with` records, reactive operations/callbacks, coroutine plans/`coro`/`await` records, and any other function-owned Stage 2 node family. The body stores its root block, concrete function signature and parameter/capture facts, plus source `Origin` and template `FunctionId` for diagnostics. Use instance-local IDs or IDs paired with the owner instance so a reference cannot accidentally resolve into the template or another instance. Keep module/type/trait/symbol semantic IDs as stable identities; when a symbol's checked type or storage facts vary by instance, store the concrete value on the instance rather than mutating the global catalog.

Materialize in Stage 3.3 ordinal order. Reserve all instance IDs and artifact IDs first (already done), then clone each body with a per-instance old-to-new map for every arena family. Register a node's new ID before descending into its children, so shared memoized occurrences and recursive references remain shared within one instance. Do not copy a nested function's body into its enclosing instance; keep the construction site and link to the nested `FunctionInstanceId`. Preserve `Origin`, syntax identity, ordered steps, and the checked control-flow/ownership plan as provenance. A missing source node or remap is a diagnostic at its recorded origin, never a panic or silent fallback to the template.

The instance body must expose concrete target variants rather than retaining a `FunctionId` plus a deferred `TraitEvidence` recipe as the final dispatch representation. Keep the original Stage 2 recipe only as optional diagnostic provenance. Use the existing Stage 3.3 dependency/artifact records as the lookup contract: match each occurrence by owner, route, origin, and resolved key where necessary, rather than choosing the first edge with a matching function ID. Repeated calls to the same function may have different substitutions, and identical source syntax may be visited under different enclosing instances.

## Implementation sequence

### Step 1 — Freeze the instance-body schema and remap coverage

- Inventory every function-owned arena and every `CheckedType`, `CheckedEffectSet`, bound, trait argument, capture type, resource type, result type, aggregate/count, or frame field that can survive in an emitted body. Reuse the Stage 3.2 parameter-family table as a checklist, but record which fields are copied, substituted, recomputed, or excluded as template-only. Include call `steps` and defaults, product `steps` and final fields, pattern/place paths, index and indexed assignment, string interpolations, `with` scopes, reactive records, and coroutine plans.
- Define the owned body and concrete target/selection types. Make the compiler require an explicit decision for every `LoweredExpressionKind`, `LoweredItemKind`, `LoweredPlaceKind`, call target, trait-evidence shape, reactive operation, and await kind when a variant is added. Keep template and instance ID types distinct.
- Add empty-body and simple nongeneric-instance tests proving that the new root and local ID maps agree, source templates remain unchanged, and one instance can be traversed using only its owned body and shared semantic catalogs.

**Gate:** The schema covers every Stage 2 family reachable from a function body and makes the instance/template boundary visible in types and validation.

### Step 2 — Clone and substitute ordinary bodies

- Build a reusable concrete substitution operation over the resolved `SubstitutionEnvironment`, using the checker's `substitute_type` and `substitute_effect_set` semantics as the reference. Apply it to the instance signature and all owned checked metadata, including parameter and capture types, coercions, call/default expected types, product shapes and repeated counts, pattern/place types, effect rows, resource requirements, and referenced bounds. Reject leftover relevant parameters, effect variables, `Inferred`, or `Error` at the occurrence origin.
- Clone blocks, items, expressions, patterns, places, calls, callable values, and resource provider/use records through per-family maps. Preserve ordered `LoweredCallStep` and `LoweredProductStep` sequences and final slot mappings; do not rebuild them from source syntax. Preserve `LoweredCallEnvironment::Current`, `LoweredClosureEnvironment`, capture order/access, mutation/move masks, initialization checks, and scope-exit facts unless a concrete-sensitive rule explicitly recomputes a derived value.
- Treat generic local bindings as compile-time template declarations, matching Stage 3.2 relevance and Stage 3.3 traversal. Clone their runtime initialization state where needed, but do not traverse or emit their generic value as a runtime body.

**Gate:** Signature-only, result-only, body-only, effect-only, repeated-product, default-expression, nested closure, and cross-module fixtures yield separate concrete bodies for different substitutions; identical requests still share one instance.

### Step 3 — Bind every dispatch site to graph identities

- Resolve direct calls and callable values to their interned `FunctionInstanceId` at the exact use site, including `Current` recursion, captured closures, implicit thunk arguments, and compiler-selected formatter constructor/finish calls. Keep indirect closure calls indirect, with their concrete callable ABI and evaluated callee. Preserve extern, intrinsic, and ordinary constructor routes; constructor values refer to their `ConstructorAdapterKey` artifact.
- For trait calls and callable values, `Index`, indexed mutation, and formatting interpolations, substitute the site recipe under the enclosing instance environment and use Stage 3.2 selection to bind either the selected explicit method instance or the existing `StructuralMethodKey` artifact. Preserve the completed trait arguments and callable type needed by the generated body. An unresolved `DeclaredBound` or rejected implementation in a concrete body is an error at the site origin.
- Bind derived evaluators, reactive callbacks, coroutine body thunks, and child coroutine links to the already interned instance/plan identities. Carry Stage 4 compiler-helper requests as explicitly unresolved artifact references with their requester and origin; do not manufacture helper bodies in this step.
- Reconcile every bound target with Stage 3.3 dependencies, including multiple occurrences that deduplicate to one instance. Do not change ordinals or enqueue newly discovered source instances silently. If materialization exposes a missing request, diagnose the missing Stage 3.3 traversal route and fix the worklist.

**Gate:** Each direct or trait-dependent use has exactly the matching existing instance/artifact; recursion is a back-edge, and indirect/external/intrinsic routes create no source-function instance at invocation.

### Step 4 — Recompute concrete-sensitive derived facts

- Identify fields whose value can change after type substitution: copy/drop decisions, capture destruction, discarded expression and replaced-value drops, temporary storage, aggregate/repeated shape, resource ABI and effect-row requirements, and coroutine frame/awaited-result metadata. Recompute only those from concrete checked values and owned trait/type catalogs, using the same rules as checking. Keep source-level move/mutation and evaluation decisions fixed.
- For nominal types, use the semantic type catalog plus concrete arguments to calculate needed layout or ownership facts without embedding recursively expanded representation trees in instance keys or bodies. Guard recursive types and retain compact nominal references.
- Compare concrete-sensitive results with the legacy checked/codegen path in tests. If a fact cannot be derived from owned metadata, add the narrow missing checked fact to Stage 2 lowering rather than consulting `TypedModule` while materializing.

**Gate:** Generic copy versus drop, captured move-only values, product defaults/spreads, effect-polymorphic resource calls, and coroutine cleanup/frame fixtures agree with legacy behavior.

### Step 5 — Validate, compare, and document the handoff

- Add an exhaustive instance-body validator: local ID ownership and bounds; concrete checked values/effects; complete target/evidence references; dependency and artifact-edge agreement; capture and resource order; coroutine plan/await ownership and resume states; no template-only deferred variants. Run it after materialization inside `Lowerer::lower` without changing the legacy LLVM path.
- Test direct and indirect calls, curried/result-only generics, same-signature captures, shared trait defaults, conditional/structural selections, string templates, constructors, nested thunks, recursion, defaults, repeated products, reactive callbacks, and demand-driven coroutine bodies. Compare two runs of the same program for stable instance-body snapshots; check that cloning one specialization cannot mutate another or the template.
- Run `cargo fmt --all -- --check`, `cargo check --workspace`, `cargo test --workspace`, and `git diff --check`; exercise CLI LLVM, object, and run paths with this worktree's standard library. Record actual results in this plan, `STAGE_3_SPECIALIZATION_BREAKDOWN.md`, and `TYPED_LOWERING_PLAN.md` after implementation.

**Gate:** Every reachable source instance has a validated concrete body; Stage 3.5 can inspect graph completeness and compare it with legacy emission without reconstructing type arguments, call targets, or trait selections from templates.

## Boundaries to preserve

- Stage 3.4 materializes reachable source-function instances. Stage 4 generates constructor/structural/compiler-helper artifacts and closes their dependencies; Stage 5 switches LLVM to the lowered graph. Keep the old backend and `TypedModule` bridge operational until that migration.
- Module initializer bodies are Stage 3.3 roots and may continue to use their owned nongeneric Stage 2 records during this step; make their source-function/artifact references available to the later graph validator and emitter. Do not treat an initializer as a generic function instance.
- Stage 3.5 owns the final fixed-point and legacy-emission comparison. Stage 3.4 must still validate its local concrete-body invariants and report a missing Stage 3.3 dependency when one is discovered.
