# Stage 2.6 Plan: Lower Resources, Reactive Operations, and Coroutines

## Goal and starting point

Replace the remaining `Deferred(Resource)` and `Deferred(Coroutine)` expressions with owned, typed operations. Record resource binding, reactive lifecycle, and coroutine suspension decisions in lowered IR so later specialization and backend migration do not have to rediscover them from syntax or `TypedModule` side tables.

Stage 2.2 already catalogs runtime functions, implicit thunks, symbols, selected subsystem IDs, and entry resources. Stage 2.3 owns runtime items and assignment places; Stage 2.4 owns ordinary expression children and control flow; Stage 2.5 owns calls, callable values, checked effect-resource requirements, and closure construction. Build on those records without changing source behavior, ABI, or the private legacy backend bridge.

## Completion gate

- Every `Resource`, `With`, `Coro`, and `Await` occurrence has one owned operation; no Stage 2.6 deferral remains in accepted programs.
- Resource reads, places, and hidden effect arguments identify a lexical provider or function/entry resource parameter using stable lowered identities. Requirements retain checked effect-row order and mutability.
- Reactive signal/derived/reaction/batch/`until` operations, scope lifetimes, and entry initialization have the semantic inputs currently used by code generation, including callback thunk relationships.
- Every coroutine body owns a plan linked to its body function, captures, deferred effects, result type, ordered await sites, frame bindings, and cancellation classifications. Each `await` has an explicit kind and resume-state relationship.
- Validation, transition comparisons against `TypedModule`, deterministic snapshots, focused regressions, and the full workspace suite pass. Stage 2.7 remains responsible for whole-program coverage and final transition checks.

## Scope and boundaries

This stage records compiler semantics, not LLVM frame layout, runtime pointers, generated symbol names, or concrete function instances. Stage 3 specializes generic templates, Stage 4 catalogs generated resume/cleanup and reactive helper artifacts, and Stage 5 migrates emission off the legacy bridge. Preserve the backend's current resource matching rule (nearest active provider with the same checked value type), evaluation order, statement-position restriction on `await`, and cancellation behavior; do not silently redefine source-language resource identity or scope cleanup.

Treat the existing Stage 2.5 call resource entries as ordered *requirements*. This stage resolves them to the provider visible at that occurrence, including implicit thunk calls and deferred coroutine effects, rather than replacing their order with a set. A function's effect resources and entry IO/reactive resources are scope roots; a `with` expression introduces a nested provider. Where a provider cannot be resolved for an accepted program, produce a source diagnostic.

## Proposed lowered records

Exact Rust names can vary, but the following facts must be owned and linked by typed arena/catalog IDs:

- `LoweredResourceProvider`: stable provider identity; checked `CheckedResource`; source or function/entry origin; provider expression or parameter; lexical parent/owner; mutable/indirect and copy-or-borrow facts; and scope-exit classification (`Reactive`, `Tasks`, or ordinary). `LoweredResourceUse`: required resource, selected provider, read versus mutable place versus hidden argument, and pass mode.
- `LoweredWith`: provider-value expression evaluated before scope entry, nested body block, provider identity, and ordered enter/exit obligations. Exits cover normal completion and existing early-exit/ownership paths without prescribing LLVM cleanup blocks.
- Reactive records: signal creation/read tracking/notification associated with the checked symbol and its storage; derived evaluator function and captures; reaction, batch, and `until` callback function or callable occurrence, checked callback type and resource bindings, selected ambient reactive scope, and operation-specific lifecycle facts. Entry initializer records retain IO/reactive creation order.
- `LoweredCoroutinePlan`: body syntax and implicit thunk `FunctionId`, ordered captures, checked result/deferred effects, resume-point count, ordered frame-binding symbols, ordered awaited-result types, and wait/`until` cancellation state classifications. `LoweredCoro` links creation to this plan; `LoweredAwait` links an operand, result type, owning plan, one-based resume state, and child-coroutine/`Task`/`Wait` kind with deferred child-resource bindings where applicable.

Use the existing expression `Origin` and checked type/effect headers. Do not clone source `Expression` or `Block` nodes into new records. Keep representation decisions that depend on concrete target layout out of Stage 2.6.

## Progress

- Step 1 complete: backend resource/reactive/coroutine decisions inventoried; owned provider, use, `with`, reactive-operation/callback, coroutine-plan, `coro`, and `await` records and arenas added with typed links; expression dispatch, intrinsic routing, arena validation, loop-exit traversal, and ownership traversal extended; route-table coverage tests added.
- Step 2 complete: function effect rows and executable-entry resources seed stable lexical providers; `with` evaluates its provider once and records place-backed versus materialized storage, borrow/indirect facts, and `Reactive`/`Tasks`/ordinary scope exits; `resource` reads and resource assignment places bind the nearest matching provider by checked value type and become explicit resource uses; validation checks provider kind/target agreement, same-owner nesting, use/provider type and indirectness agreement, `with` value and scope-exit agreement, and mutable-place legality; focused shadowing/storage/scope-exit/corruption tests plus CLI resource and reactive compile-run tests pass.
- Step 3 complete: every call's hidden effect-row requirements resolve to selected providers in checked order and become ordered `HiddenArgument` resource uses with borrow/value passing facts; external, intrinsic, and constructor routes stay unbound because they take no hidden resource ABI arguments, and unresolved generic effect templates keep no invented provider. Missing/non-borrowable providers diagnose, call-resource validation compares ordered bindings with the checked effect row, and focused order/scope/mutable/template tests pass.
- Step 4 complete: signal bindings own `SignalCreate` (global versus local-cell storage), signal reads and assignments own `SignalRead`/`SignalNotify`, derived bindings own `DerivedCreate` with evaluator thunk, checked callback type, ordered captures, and resource-free validation, and derived reads own `DerivedRead`. Reactive intrinsic calls (`reactive_scope`, `reaction`, `batch`, `until`, `snapshot`) own explicit operations with callback thunk-or-callable identity, checked callback type, ordered captures and hidden callback resources, selected ambient `Reactive` provider, and `until` predicate purity. Validation checks attachment kinds and reachability; focused signal/derived/reaction/batch/until/snapshot and purity tests pass.
- Steps 5-7 pending.

## Implementation sequence

### Step 1 — Inventory subsystem decisions and establish typed relationships

- Trace `Resource`/`With`, `compile_resource_arguments`, signal and derived paths, reactive intrinsics, `compile_coro_expression`, `compile_coroutine_await`, and `coroutine_lower::plan` to list every checked input and current backend-only decision.
- Define provider, use, reactive, and coroutine-plan IDs/records with typed links to existing expression, block, call, item, function, and symbol catalogs. Add narrow checked side tables only for decisions not reproducible from stable checked metadata; do not move LLVM implementation details into the IR.
- Extend expression dispatch, arena reference validation, and ownership traversal for the new records before populating them. Add a route table test covering every remaining deferred syntax family and each reactive/coroutine intrinsic route.

Gate: the schema can represent every backend route with no untyped fallback or source-AST payload.

After this step, update `TYPED_LOWERING_PLAN.md` with the most general progress and `STAGE_2_LOWERING_BREAKDOWN.md` with detailed results and verification.

### Step 2 — Lower lexical resource providers and accesses

- Seed function and module-entry resource parameters from checked effect rows and Stage 2.2 initializer metadata. Give each active provider a stable identity and retain checked value type, mutability, lexical owner, and source origin.
- Lower `with` in backend order: evaluate the provider value once, establish the provider for its body, then leave its scope. Record place-backed versus materialized provider storage and borrow/copy facts without embedding LLVM pointers.
- Replace `Resource` reads and resource places with uses bound to the nearest matching active provider by checked value type. Preserve mutable-place legality and diagnostics; distinguish a read from an addressable resource place.
- Capture `Reactive` and `Tasks` scope entry/exit classifications and the cleanup boundary for normal and early exits. Validate provider visibility, nesting, type agreement, and unique ownership.

Gate: nested/shadowed resources, mutable and non-`Copy` providers, resource reads/assignments, function effects, and entry resources bind to the same provider the legacy backend would use.

After this step, update both plan files as above.

### Step 3 — Bind ordered effect resources at use sites

- Resolve Stage 2.5 hidden call requirements in checked effect-row order, after visible argument evaluation as already recorded by call steps. Attach selected provider IDs and value/pointer passing facts to each requirement.
- Cover direct, indirect, trait-dispatched, intrinsic, and implicit-thunk calls, including reaction/batch callbacks. Retain template effect parameters where Stage 3 must substitute them; do not invent a concrete provider for an unresolved generic requirement.
- Record deferred resource acquisition for child coroutine activation separately from `coro` frame creation. Preserve the current distinction between coroutine creation and the `await` that supplies a child's effect bundle.
- Diagnose missing, mismatched, or out-of-scope providers and validate ordered requirement-to-binding cardinality.

Gate: no resource-bearing call or child-coroutine activation needs a backend lexical search to identify its checked provider.

After this step, update both plan files as above.

### Step 4 — Lower signal, derived, and reactive lifecycle metadata

- Attach signal create, tracked read, write notification, and storage metadata to lowered binding/name/assignment sites, respecting global versus local initialization and the existing signal symbol classification.
- Link each derived binding to its selected evaluator thunk, checked callback type, ordered captures, and resource-free requirement. Retain creation versus later read/recompute semantics.
- Attach explicit reactive operation records to `ReactiveScope`, reaction, batch, and `until` intrinsic calls. Record callback expression versus implicit thunk, callback resources, selected ambient `Reactive` provider, batching boundaries, and `until` predicate purity/cancellation relationship.
- Preserve module-entry IO/reactive initialization order and `with Reactive`/`with Tasks` lifetime obligations. Keep runtime allocation and subscription layout for Stage 5.

Gate: the lowered program identifies every reactive callback and scope decision used by the current backend, with focused tests for signals, derived bindings, nested scopes, reaction, batch, `until`, and entry initialization.

After this step, update both plan files as above.

### Step 5 — Own coroutine plans and body/thunk relationships

- Copy `coroutine_lower::CoroutinePlan` into deterministic lowered records keyed by body syntax and owning implicit thunk function. Preserve ordered captures, frame-binding symbols, await-result types, resume count, and wait/`until` state classifications exactly.
- Link the body block and its function catalog entry, including checked result and deferred effect row. Validate that plan captures agree with the thunk capture catalog and that every frame symbol is owned by the coroutine body.
- Preserve the scanner's rule that nested `coro` and function bodies are separate owners. Keep rejected non-statement-position `await` as a source diagnostic rather than silently spilling arbitrary temporaries.

Gate: each coroutine has one complete plan and no coroutine metadata used for frame/resume/cleanup emission must be fetched from the legacy plan map.

After this step, update both plan files as above.

### Step 6 — Lower `coro` creation and ordered `await` operations

- Replace `Coro` deferrals with explicit creation operations linked to the body plan and capture construction. Creation records frame-rooting and deferred-effect ownership facts but not target-specific frame offsets.
- Replace `Await` deferrals with ordered sites in their owning coroutine plan. Distinguish child-coroutine, external `Task`, and external `Wait` operands; retain checked outcome/result types, one-based resume state, child-resource requirements, and pending-result semantics.
- Align wait and `until` cancellation classifications with the current cleanup rules: abandoned completion records, safe child cleanup for `until`, and driver-owned cleanup for other queued children. Validate state ranges, one-to-one await-site mapping, and statement position.

Gate: every accepted `coro`/`await` expression has an owned operation whose plan reconstructs the backend's suspension and cancellation decisions without rescanning syntax.

After this step, update both plan files as above.

### Step 7 — Close validation and transition coverage

- Reject remaining `Deferred(Resource)` or `Deferred(Coroutine)` expressions. Validate provider scope/use consistency, effect-resource ordering, reactive callback and evaluator identities, coroutine body/thunk ownership, capture order, await-state sequence, and all arena/catalog links.
- Compare lowered provider selections, checked resources, implicit thunk links, signal/derived metadata, entry reactive requirements, and coroutine plans with `TypedModule` during the legacy transition. Extend normalized repeated-lowering snapshots to new records.
- Run focused lowering and diagnostic tests, `cargo fmt --all -- --check`, `cargo check --workspace`, `cargo test --workspace`, `git diff --check`, and representative CLI compile/run, object-emission, and LLVM verification paths.

Gate: Stage 2.6's four syntax families and reactive/coroutine side decisions are fully represented; Stage 2.7's global coverage audit is the next step.

After this step, mark Stage 2.6 complete in both plan files and name Stage 2.7 as next.

## Regression matrix

- Resources: nested same-type shadowing, different-type coexistence, mutable place, non-`Copy` borrowing, materialized provider, missing provider, entry IO/reactive roots, call requirement order, and generic effect templates.
- Reactive: local/global signal creation, tracked read, mutation notification, derived evaluator/captures, reaction versus batch callback forms, `until` purity and subscription cancellation, and `Reactive`/`Tasks` scope exits.
- Coroutines: captured values/cells, deferred effects, nested bodies, multiple statement-position awaits, child coroutine versus `Task`/`Wait`, frame binding order, awaited result types, pending external outcome, `until`/wait cancellation, and invalid expression-position awaits.
- Transition: repeated-lowering determinism, source-based diagnostics for missing links, and no behavior or ABI change while LLVM still uses the private legacy bridge.
