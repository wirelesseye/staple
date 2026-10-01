# Stage 5.8 Plan: Coroutines, Tasks, and Reactive Code

This is the separate plan the Stage 5.8 section of [STAGE_5_LLVM_MIGRATION_BREAKDOWN.md](STAGE_5_LLVM_MIGRATION_BREAKDOWN.md) requires. First read:

- the breakdown's Migration Contract (items 1–7) and Decisions D1–D6, especially D2 (planned names), D5 (mirrored defects), and D6 (runtime gating);
- the Stage 4.5 plan ([STAGE_4_5_COROUTINE_AND_REACTIVE_ARTIFACTS_PLAN.md](STAGE_4_5_COROUTINE_AND_REACTIVE_ARTIFACTS_PLAN.md)), especially "Legacy behavior to capture", the final schemas, the latent defects, and the Stage 5 hand-off in Step 7;
- the post-gate review fixes in the 5.6 and 5.7 plans and sections (drop machinery, early-exit reactive disposal, recorded intrinsic facts).

This plan's gate (Step 10) supersedes the **Gate** paragraph in the breakdown's 5.8 section.

Line references are against `f13c2f3` and will drift; re-locate code by function name. Run the suite with `cargo nextest run --workspace`.

## Starting Point

The differential corpus (49 programs) compares 13935 fully emitted bodies and reports 74 stubs, all owned by 5.8. Every other substage is in the zero-stub ratchet (`COMPLETED_SUBSTAGES`). The first blockers:

| Family | Stubs | Notes |
| --- | --- | --- |
| `coroutine pair artifact` | 28 | `resume`/`cleanup` bodies, including the standard library's |
| `coro` | 14 | the creation expression |
| `checked or reactive name` | 12 | signal and derived reads |
| `until runner artifact` | 5 | |
| `reactive or cell binding` | 4 | signal and derived binding items |
| `derived runner artifact`, `reaction runner artifact` | 3 + 3 | |
| `completion`, `scheduler`, `signal notify`, `task cancel`, `until call` | 1 each | |

Each stub records only its body's **first** blocker. Behind these sit `await`, the remaining task, scheduler, and completion intrinsics, `reaction`/`batch`/`snapshot` calls, the `with Tasks` scope (diagnosed in `emit_with`), and the `CompletionOrphan` drop site. They surface as the families above are emitted.

The CLI harness reports 43 identical, 3 blocked (`example_coroutines`, `example_signals_and_reactions`, `example_game_loop`), and 3 compile-only programs.

What already exists:

- **Declarations.** The pair (`resume`, `cleanup`) and the three runner families are declared under their planned names (5.3). Coroutine body-thunk instances are deliberately **not** declared: legacy compiles the body inline in `resume`, and so must 5.8.
- **Lowering records.** Stage 4.5 closed the catalog:
  - `CoroutineCodesPlan { body, frame: CoroutineFramePlan }` (result type, resume points, frame bindings with `unwind_drop`, await result types, `Wait`/`until` states, resource slots with pass modes, captures, `capture_finalizer`);
  - `ReactiveRunnerBody::{Reaction, Until, Derived}`;
  - the use sites `CoroCreation`, `ReactiveCallbackEnvironment`, `DerivedEvaluatorEnvironment`, `ReactiveRunner`, and `CompletionOrphan`;
  - `LoweredAwait { resume_state, kind: ChildCoroutine | Task | Wait }`, `LoweredReactiveOperationKind`, `LoweredReactiveCallback`, `LoweredSignalStorage`, and `LoweredScopeExit::Tasks`.
- **Shared backend pieces.** These are the frame header constants (`codegen/layout.rs`, `CORO_*`), `build_coroutine_frame_cleanup`, `build_runtime_release`, `build_reactive_runtime_call`, `dispose_reactive_scopes`, `emit_drop_glue`, the owned-scope machinery, and the conditional cell drop.

The legacy code this substage ports (sizes in lines):

| Legacy function | Lines |
| --- | --- |
| `ensure_coroutine_codes` + `coroutine_frame_layout` + `store_coroutine_resources` | 727 |
| `compile_coro_expression` | 69 |
| `compile_coroutine_await` + `compile_external_await` | 438 |
| `compile_coroutine_drive` | 86 |
| `compile_scheduler_intrinsic` + `coroutine_current_scheduler` + `coroutine_current_task_scope` + `close_task_scopes` | 330 |
| `compile_completion_intrinsic` | 274 |
| `compile_reaction` + `compile_batch` + `compile_until` + `emit_until_runner` | 604 |
| `compile_derived_create` + `force_derived_read` + `signal_metadata_value` + `track_signal_read` | 246 |

**Legacy nondeterminism.** Legacy `coroutines.sta` IR differs on nearly every run. Two `HashMap` iterations cause it:

- `CoroutineFrameLayout::cell_fields` decides the order of `resume`'s frame-cell prologue and its unwind drops (`ensure_coroutine_codes`, three loops).
- `self.storage` decides `main`'s global GC-root registration.

The lowered emitter is deterministic: plan order for frame cells and unwind drops, and symbol order for roots. As things stand, a legacy-body comparison of `resume` would fail at random.

**Legacy's type queries.** These are the `TypedModule` queries the ported legacy code makes. Each is a fact the lowered emitter must read from a record (Contract 1):

| Legacy query | Used for | Lowered source |
| --- | --- | --- |
| `type_of_symbol`, `type_needs_drop` (`ensure_coroutine_codes`) | frame cell types, unwind drops | `CoroutineFrameBinding { value_type, unwind_drop }` |
| `active_type_substitutions` (pairs, `coro`) | concrete types | the body instance and the pair plan (already concrete) |
| `task_result`, `wait_result` (`compile_coroutine_await`) | external await payloads | `LoweredAwaitKind::{Task, Wait}.result` |
| `wait_result`, `concrete_expression_type`, `type_needs_drop` (`compile_completion_intrinsic`) | completion value type, orphan drop | `LoweredCall.runtime.completion_value_type` and the `CompletionOrphan` use |
| `type_of_function`, `concrete_expression_type` (`compile_reaction`, `compile_batch`) | callback closure type | `LoweredReactiveCallback.function_type`, `ReactiveRunnerBody::Reaction` |
| `concrete_expression_type` (`compile_scheduler_intrinsic`) | intrinsic argument types | `LoweredCall.arguments[*].expected` and `runtime.coroutine` |
| `is_tasks_type` (`coroutine_current_task_scope`) | finding the `Tasks` resource by type | `runtime.coroutine.tasks_resource` and the call's `resource_bindings` |
| `is_signal_symbol`, `is_derived_symbol` | signal metadata, forced derived reads | symbol flags and `LoweredReactiveOperationKind` |

## Decisions Specific to 5.8

**K1: Pairs come from the body instance plus the frame plan.** `resume` and `cleanup` are emitted from the artifact's `CoroutineFramePlan` and its `body` instance:

- the instance's lowered body block, with owner `EmissionOwner::Instance(body)`;
- its local coroutine plan (`body.plans[0]`), for the awaits;
- its captures.

The emitter never reads the template plan that `LoweredCoro.plan` indexes, and never derives a frame fact from types. The backend keeps the LLVM layout decisions: the header struct, frame-cell types from `binding_cell_type`, the result field, and pending-slot sizing from `await_result_types`.

**K2: One order everywhere, and legacy is made deterministic first.** Frame cells, the `resume` prologue, and unwind drops all follow `frame_bindings` (plan) order. Rather than teaching the harness to compare permutations, Step 1 makes legacy iterate frame cells in plan order and `main`'s roots in symbol order:

- This is a determinism change, not a behavior change. Each new legacy output is one of the variants legacy could already produce.
- It closes the "plan order, never legacy `HashMap` order" note in D5 for this family.
- Every later step can then require `same` from the legacy IR comparison, including `coroutines.sta`.

**K3: Contract 1 for intrinsic facts.** Following the 5.4–5.7 reviews, every per-call type fact legacy queries (the table above) must come from a lowering record. A missing fact is added in lowering, recomputed per instance, and validated, with a corruption test, as `LoweredCall::buffer_pop` was in 5.7. It is never re-derived in the emitter.

**K4: Mirror the D5 defects and legacy's exit behavior.**

- **Completed-coroutine leak.** A completed body does not drop its frame bindings; only the cancel unwind drops them. 5.11 fixes this.
- **Generic aliasing.** Legacy reuses one syntax-keyed pair (and `until` runner) across instantiations, or fails on them. The lowered emitter emits one pair and runner per instance by construction. These are the recorded D5 differences: the census explains them as aliased artifacts, and the CLI harness checks only the lowered emitter's output for these programs (Step 8).
- **Task scopes close only on a normal exit.** Legacy `close_task_scopes` runs only at a `with Tasks` normal exit, not on `return`/`break`/`continue`, unlike reactive scopes. Mirror it, and record it in the handoff as a 5.11 candidate. Do not "fix" it by analogy with the 5.6 reactive-disposal review fix.

**K5: Coroutine-body ownership follows the 4.5 reconciliation.** Frame bindings are frame cells and never owned (4.5 gap 2). Every other binding in the body uses the 5.6 owned-scope machinery unchanged, including function-end `drop_all_owned` before the result store. Awaits are suspension points and run no scope drops. The cancel unwind uses only the plan's `unwind_drop`s.

**K6: One plan, two tracks.** The coroutine track (Steps 3–6) and the reactive track (Step 7) are independent until the `until`-inside-a-coroutine await, which needs both. Step 7 can run in parallel with Steps 4–6 once Step 3 lands.

## Steps

Each step ends with the Contract 6 gates (`cargo nextest run --workspace` for the suite) and a commit.

- **Shared-helper steps** prove legacy IR unchanged with `scripts/compare-llvm-ir.py` against the pre-step binary. Run it over `staple-compiler/examples/*.sta`, `examples/game_loop/main.sta`, and the 5.6/5.7 probe programs, plus `coroutines.sta` at 16 runs, which must now be a single variant.
- **Every emission step** reports the harness numbers (bodies compared, stubs per family) in its notes.

### Step 1: Hygiene, legacy determinism, and true blockers

- **Legacy determinism (K2).**
  - Make `CoroutineFrameLayout::cell_fields` an ordered list in `frame_bindings` order, and iterate it in the three `ensure_coroutine_codes` loops.
  - Iterate `main`'s global roots in symbol order.
  - The comparison against the pre-step binary must show every new output among the old variants (raise `--runs` until it does), and the new binary must produce exactly one variant per program at 16 runs.
  - Record the change in the breakdown under D5.
- **Retire the placeholder families.**
  - `deferred expression` and `Stage 2.6 expression` cannot reach emission: the instance validator rejects them, so confirm the initializer path rejects them too and add the check if it does not.
  - Turn the emitter arms into internal-invariant errors and remove both from the ownership table.
- **Split the families for tracking.**
  - `checked or reactive name` → `signal read`, `derived read`.
  - `reactive or cell binding` → `signal binding`, `derived binding`.
  - Give the `emit_with` task-scope diagnostic the `task scope` family.
- **Measure the true blockers.** Add a test-only mode to the partial report that records every construct a stubbed body reaches, not only the first. Record the full per-family census in the step notes, so the later steps have a complete list rather than discovering families one at a time.

**Step 1 notes (complete).** Legacy `CoroutineFrameLayout::cell_fields` is now an ordered `(SymbolId, field index)` list built from `frame_bindings`; all three consumers retain that order. `main` sorts global roots by symbol ordinal. A 16-run `coroutines.sta` comparison produces exactly one new variant contained in the pre-step variant set. `scripts/compare-llvm-ir.py --new-subset` checks this determinism transition explicitly without relaxing the default equal-variant-set comparison used by subsequent helper steps.

The initializer/program-arena `validate_arena_references` already rejects every `Deferred` and `Stage26Deferred` route; the instance validator does likewise. Their emitter arms now report an internal invariant error, and neither placeholder family remains in `FAMILY_OWNERS`. Reads and bindings diagnose separate signal/derived families using the recorded operation or binding flags. The `emit_with` diagnostic already uses the standardized `task scope` family.

Test-only partial emission now walks the complete runtime owner of every stubbed function through the existing lowered owner walker. Coroutine pair stubs walk their body instance; both pair functions are counted, since each is a stubbed function. Counts below are functions reaching a family (one count per family per function), not occurrence counts. The first-blocker report remains unchanged: **13935 fully emitted bodies compared, 74 stubs across 12 families**, all owned by 5.8. The complete reached-family census has 24 families:

| Family | Stubbed functions reaching it |
| --- | ---: |
| await | 18 |
| batch call | 1 |
| completion | 1 |
| coro | 14 |
| coroutine pair artifact | 28 |
| derived binding | 3 |
| derived read | 1 |
| derived runner artifact | 3 |
| pump | 3 |
| reaction call | 3 |
| reaction runner artifact | 3 |
| resolver complete | 1 |
| scheduler | 2 |
| signal binding | 4 |
| signal notify | 4 |
| signal read | 18 |
| snapshot call | 1 |
| spawn | 2 |
| task cancel | 2 |
| task is_finished | 1 |
| task scope | 2 |
| until call | 7 |
| until runner artifact | 5 |
| yield_now | 2 |

Gates: both full workspace test gates pass (1301 tests in nextest and `cargo test --workspace --quiet`); default and lowered-feature workspace checks, the test-target check, formatting, and diff checks pass. The 16-run example sweep produces one new variant contained in the old set for every compiling program; `coroutines.sta` reduces four old variants to one. `macros.sta` remains rejected by both binaries. The 13 extracted 5.6/5.7 probes match at two samples per binary. The plan's `examples/game_loop/main.sta` reference is stale: the actual fixture is `staple-compiler/examples/game_loop/main.sta`, checked separately at 16 samples. Legacy coroutine LLVM/object/run paths and lowered-feature hello-world LLVM/object/run paths pass with the worktree standard library. Subsequent shared-helper steps compare against the deterministic legacy binary with the default equal-set mode.

### Step 2: Lowering facts (K3)

For each row of the type-query table marked "verify", show that the lowered record carries the fact, or add it.

- **Completion intrinsics.**
  - The completion value type that legacy reads through `wait_result`/`concrete_expression_type`, for the slot size and the `ResolverComplete` store.
  - The orphan drop's existence comes from the `CompletionOrphan` use, never from `type_needs_drop`.
- **Scheduler and task intrinsics.** The `Tasks` provider for `spawn`, `yield_now`, and the other scheduler intrinsics must come from the call's `resource_bindings` (the hidden effect-row resources), not a type search over the environment. If any intrinsic lacks the binding, record it.
- **`reaction`/`batch` callback types** come from `LoweredReactiveCallback.function_type`. Confirm that each instance re-substitutes them.
- **Child awaits.** `LoweredAwaitKind::ChildCoroutine.plan` may be `None` ("only the checked effect/result parts are known"). Confirm that legacy's child await never needs the plan in that case, or record what it reads.

Each added fact is recomputed per instance and checked by a validator, with a corruption test. **Gate:** the step notes list each type query and its record, and no "verify" entry remains open.

**Step 2 notes (complete).** `LoweredCall::runtime` now records `LoweredRuntimeCallFacts`: a completion payload type and, for `spawn`/`block_on`, a `LoweredCoroutineActivation` with the concrete result type, ordered deferred-resource indices, and the `Tasks` resource index. These indices address the call's `resource_bindings`, whose uses name exact lexical providers and carry pass modes. Both ordinary and juxtaposed intrinsic calls retain the activation effect row. Instance cloning substitutes argument/signature types, rebinds the concrete row, rebuilds intrinsic resource steps when the row changes, and recomputes the facts. Program-arena and instance validators recompute and compare the records; the resource validators require the entire recorded effect row. Intrinsic resource steps are metadata and do not add hidden runtime ABI arguments.

The query audit is closed:

| Legacy query | Recorded source and validation |
| --- | --- |
| `type_of_symbol` / `type_needs_drop` for frame bindings | `CoroutineFrameBinding.value_type` / `unwind_drop`, re-expanded by the 4.5 pair validator |
| `active_type_substitutions` for pairs and creation | concrete body instance, captures, and frame plan |
| `task_result` / `wait_result` for external awaits | `LoweredAwaitKind::Task/Wait.result`, with checked `LoweredAwait.result_type` |
| `wait_result` for completion creation and `concrete_expression_type` for resolver completion | `LoweredCall.runtime.completion_value_type`, recomputed after substitution and checked in both owner shapes |
| `type_needs_drop` for a completion orphan | presence of the exact `CompletionOrphan` artifact use; the 4.4 scanner selects glue from the last argument's recorded expected type |
| callback `type_of_function` / `concrete_expression_type` | `LoweredReactiveCallback.function_type`; `BodyCloner::clone_callback` substitutes it per instance, the body validator checks concreteness, and 4.5 re-expands runner plans from it |
| scheduler `concrete_expression_type` / coroutine parts for `spawn` and `block_on` | `LoweredCall.arguments[*].expected` and `runtime.coroutine.result_type` / ordered deferred resources |
| `is_tasks_type` for `spawn` | `runtime.coroutine.tasks_resource` indexes the recorded call binding, never an environment type search |
| signal / derived predicates | lowered symbol flags and the explicit reactive operation |

`yield_now` allocates only a header and records no ambient provider at creation: its `Tasks` requirement is deferred until activation/await and is already carried by that site's resource records. Scheduler creation, task-scope creation, pump, task queries/cancellation, and completion handle operations use their visible recorded arguments; legacy searches no ambient `Tasks` provider for those operations. Reaction/batch callback records and their instance substitution needed no new field. A child await with `plan = None` needs no missing fact: legacy reads the child's checked deferred row/result, stores the resource bundle, returns the child frame to the driver, and loads the pending result, reaching resume/cleanup only through header slots.

Six new tests prove generic completion payloads become concrete at I32/U8 instantiations, IO versus pure coroutine activation rows and exact providers, corruption of completion payloads in both template and instance records, corruption of activation result/deferred/Tasks facts, missing intrinsic bindings, and a parameter child await with no body plan. The focused tests and all Contract 6 gates pass: nextest and `cargo test --workspace --quiet` each pass 1307 tests, both workspace checks pass, formatting/diff checks pass, legacy coroutine and lowered-feature hello-world LLVM/object/run paths pass, and the legacy IR sweep against the Step 1 deterministic binary reports `same` (one variant) over every compiling example and the actual game-loop fixture at two samples per binary (`macros.sta` remains rejected by both). The differential corpus is unchanged: **13935 fully emitted bodies compared, 74 stubs across 12 first-blocker families; 24 reached families**. No type-query verification entry remains open. Step 3 is next.

### Step 3: Shared coroutine and reactive helpers (Contract 7)

Move the `TypedModule`-free cores into `codegen/ir.rs` (or `codegen/runtime.rs` for runtime-call wrappers) and switch legacy to them:

- **Frame layout.** The header plus cells plus result plus pending-array struct, built from already-compiled types. Legacy keeps `coroutine_frame_layout`'s type queries; the shared core takes the field types.
- **Frame header.** Initialization and the status struct, state load/store, and the `DONE`/`FREED`/`CANCELLED` constants and tests.
- **GC root registration** of the frame, and its unregistration in `cleanup`.
- **The `resume` skeleton.** The cancellation test, the state `switch` over `0..=resume_points`, and the bad-state trap.
- **The await suspension skeleton.** Store the state, return the pending status, and the resume block; plus pending-result store and load.
- **Runtime-call cores.** Scheduler/task, completion (`__staple_completion_*`), and reactive (`__staple_reaction_create`, batch begin/end, `until` resume/cleanup, `__staple_derived_create`, signal create/track/notify), each taking pointers and values only.
- **Runner payloads.** The reaction payload struct with per-slot pass mode, the fixed `until` payload, and the derived `{evaluator, output}` payload, plus each runner's indirect-call core.

The type recursion and the `TypedModule` reads stay in legacy. **Gate:** the legacy IR comparison reports `same` everywhere, `coroutines.sta` included, as a single variant.

**Step 3 notes (complete).** The compiled-value cores live in `codegen/ir/coroutines.rs`, re-exported through the shared IR layer. Legacy now uses them for frame layout and header initialization, resume entry/cancellation/state dispatch/bad-state trap, normal and cancelled returns, cleanup/root unregistration, child and external await suspension, pending results, resource bundles, synchronous driving, yield and until frames, reaction/derived payloads and runners, and batch callback calls. Header/status and task/completion layouts stay in the shared layout layer; scheduler/task/completion calls use the shared runtime-call primitive, and reactive operations retain the existing shared named-call wrapper. Checked-type recursion, capture/resource selection, finalizer selection, and source-keyed declaration caches remain in legacy. The new cores contain no checked-module or source-expression queries.

The final full nextest run and `cargo test --workspace --quiet` each pass 1307 tests. Default and lowered-feature workspace checks, formatting/diff checks, and legacy coroutine plus lowered hello-world LLVM/object/run smoke paths pass. The differential census remains **13935 bodies compared, 74 stubs across 12 first-blocker families; 24 reached families**. The Step 2 legacy binary comparison reports `same` as a single variant over every compiling example, the actual game-loop fixture, and all 13 5.6/5.7 probes at two samples per binary; `macros.sta` remains rejected by both. The final legacy binary also matches the Step 2 binary on `coroutines.sta` at 16 samples per binary, with exactly one variant. Step 4 is next.

### Step 4: Coroutine pairs and `coro` creation

- **`FunctionEnvironment` gains a coroutine context.** This mirrors legacy `CoroutineContext`: the frame, frame type, status type, per-state dispatch blocks, and pending field. `next_state` is replaced by each await's recorded `resume_state`; assert that they agree with the dispatch block count.
- **`resume`.**
  - Unpack the resource bundle from `CORO_RESOURCES` by `CoroutineResourceSlot.indirect`.
  - Bind captures from `CORO_CAPTURE_ENV` with the body instance's capture layout.
  - Point every frame binding at its frame cell (`binding_cells`), in plan order.
  - Run the cancellation check, then the state dispatch.
  - **State 0** emits the body instance's root block, with owner `Instance(body)`. On normal completion it runs `drop_all_owned` (K5), stores the result through `CORO_RESULT_PTR`, marks the frame `DONE`, and returns.
- **The cancel unwind.**
  - On a recorded `Wait` state, abandon the stashed child record.
  - On a recorded `until` state, call the child's cleanup through its header slot.
  - When `capture_finalizer` is present and the state is 0, call it on the environment.
  - Expand each frame binding's `unwind_drop` through the conditional cell drop, in plan order.
  - Mark the frame `DONE` and return `CANCELLED`.
  - The completed-body leak stays mirrored (K4).
- **`cleanup`.** Return when `FREED`. On state 0 with a capture finalizer, call it. Unregister the root and mark the frame `FREED`.
- **The `coro` expression.**
  - Resolve the pair from the `CoroCreation` use.
  - Build the capture environment without installing a finalizer (`install_finalizer = false`, as legacy does).
  - GC-allocate the frame, store the header, and register the root.
- **Partial mode** stubs both pair functions together when either fails.

**Gate:**

- `coroutine pair artifact` and `coro` reach zero.
- Every emitted `resume`/`cleanup` body matches legacy, possible only because of Step 1.
- Pairs at a recorded aliased instantiation are explained by the census rather than compared.

**Step 4 notes (complete).** `codegen/lowered/coroutines.rs` emits `CoroutineCodes` pairs from their concrete frame plan and body instance, including deferred-resource bundle slots, ordered captures and frame cells, cancellation/dispatch, capture finalizers, conditional unwind drops, normal result completion, and cleanup. Creation resolves the exact `CoroCreation` use and allocates its capture environment without installing a finalizer. The body root/coercion/move handling and capture binding are shared with ordinary instance emission; coroutine bodies use their own `Instance(body)` owner and the existing owned-scope machinery. The context stores dispatch blocks and pending layout for Step 5's recorded await states. Partial mode attempts pair bodies and replaces both functions together when the body fails. The remaining cancellation teardown skeleton was moved into the shared IR layer and legacy uses it too.

The differential harness now normalizes only pair-pointer references at census-confirmed aliased `CoroCreation` uses: the legacy pair for the same body template is renamed to the creation's concrete pair. This checks every other creator instruction and leaves the aliased pair body explained by the census; it does not suppress arbitrary body differences. A focused fixture compares empty and captured/resource-bearing pairs, including a droppable frame binding. A separate partial fixture proves an unsupported await stubs both parent functions while the child pair remains emitted (Step 5 will replace this staging-specific fixture once awaits are supported). The corpus ratchets `coro` and `coroutine pair artifact` to zero in both histograms. All three focused checks pass. Corpus: **13957 bodies compared (+22), 50 stubs (-24), 11 first-blocker families, 22 reached families**. The three older selector/strict/partial diagnostic tests now use the still-unported reaction operation; plain coroutine creation is expected to compile. Both full workspace test gates pass **1309 tests**, as do default/feature checks, formatting/diff checks, legacy coroutine and lowered captured/empty coroutine-creation LLVM/object/run paths. The Step 3 legacy binary comparison is `same` over every compiling example, the actual game-loop fixture, and all 13 probes at two samples per binary; `macros.sta` is rejected by both. The separate coroutine comparison is `same` at 16 samples per binary, exactly one variant. Every fully emitted mapped resume/cleanup body is compared by the corpus; aliased pair bodies retain the census explanation. Step 5 is next.

### Step 5: Awaits and `block_on`

- **Child coroutine await.**
  - Activate the child with its `deferred_resources`, stash it in `CORO_CHILD`, and suspend at the recorded state.
  - On resume, drive or poll the child, read its result through the pending slot, and return it.
  - The `until` flag selects the off-queue parking legacy uses.
- **External awaits** (`Task`, `Wait`): legacy `compile_external_await`, producing the `Completed T | Cancelled` outcome sum from `LoweredAwait.result_type`. `Wait` states line up with the plan's `wait_await_states`.
- **`CoroutineBlockOn`** (`compile_coroutine_drive`): the synchronous drive loop for a top-level coroutine.

**Gate:** `await` and `coroutine block_on` reach zero, and the nested, child-await, `Wait`, and `until`-state 4.5 fixtures run identically (as corpus entries, Step 9).

**Design note (Steps 4–5).** `FunctionEnvironment::coroutine` is an owned clone of `CoroutineContext`: the frame pointer, frame struct type, status struct type, the resume-state dispatch blocks (`dispatch[0]` is state 0; `dispatch[state]` is the resume block for the one-based state `state`), and the pending-field index, plus Step 4's existing capture/resource bindings. There is no `next_state`: each await's recorded `resume_state` selects its dispatch block, and the fallible `dispatch.get(resume_state)` lookup enforces agreement with the plan's `resume_points`. `emit_coroutine_pair` installs the context before emitting the body root; `emit_await` reads it, clones it, and the shared `resume`/cancellation skeleton owns `CORO_CHILD`, the pending slot, and the status returns. External awaits recover the parked record from `CORO_CHILD` in the dispatch block, exactly as legacy recovered it from its `next_state` context.

**Step 5 notes (complete).** `LoweredAwaitId` now resolves through the owner-aware emission view. `emit_await` emits both suspension kinds from the recorded `LoweredAwait`: a child await evaluates the operand, packs the recorded `deferred_resources` uses into a GC bundle through `store_coroutine_resources` (each use's pass mode decides a pointer or value slot, matching the child frame plan's `CoroutineResourceSlot.indirect`), parks `RESUME_CHILD` at the recorded state, and loads the pending result in the dispatch block; a `Task`/`Wait` external await calls `build_external_await_suspend` and then reads the parked record's state to build the `Completed payload | Cancelled` outcome sum through the recorded `LoweredAwait.result_type` and its two alternatives, reusing the shared coercion plan for the sum injection. `CoroutineBlockOn` drives the frame from the call's recorded `LoweredCoroutineActivation` (result type plus deferred-resource indices into the call's own `resource_bindings`) and returns the loaded result after cleanup. `emit_intrinsic` gained the environment so an activation can resolve its recorded providers.

The nested-await fixture replaced Step 4's staging partial fixture: two suspended pairs (the parent awaiting an inline `coro` and a named coroutine; the named coroutine awaiting its own child) plus two `block_on` drives (an owned parent and an inline coroutine) fully emit, declaration-map, and compare body for body, including the shared `resume`/`cleanup` cores. The step also fixed a parity bug the fixture exposed: `release_moved_ownership` cleared every moved symbol's binding cell state, while legacy clears only `has_mutable_storage` symbols, so a coroutine frame cell for an ordinary `let` gained an extra state store; the lowered path now reads the symbol's recorded `mutable_storage`.

Corpus: **13963 fully emitted bodies compared (+6), 44 stubs (-6), 11 first-blocker families, 22 reached families; `await` and `coroutine block_on` are absent from both histograms' first blockers**. First blockers: signal read 16, until call 7, until runner artifact 5, signal binding 4, derived/reaction runner artifact 3+3, yield_now 2, completion 1, scheduler 1, signal notify 1, task cancel 1. The external-`Task`/`Wait` and `until`-state paths are emitted but their creating constructs are 5.8 Steps 6–7; the Step 9 fixture set exercises them end to end. All Contract 6 gates pass: nextest and `cargo test --workspace --quiet` each pass 1309 tests, both workspace checks pass, formatting and diff checks pass, and the default and lowered-feature CLI `--emit llvm`, `--emit object`, and `run` paths pass on a nested-await/`block_on` program (legacy and lowered both print `work` and `answer 9`). Step 6 is next.

### Step 6: Tasks, schedulers, and completions

- **Scheduler and task intrinsics:** `SchedulerCreate`, `Spawn`, `Pump`, `YieldNow`, `TaskIsFinished`, and `TaskCancel`. Their `Tasks` provider comes from the call's recorded resource binding (Step 2).
- **`with Tasks`.**
  - Push the scope, then close it at the normal exit (`close_task_scopes` back to the mark).
  - Remove the `emit_with` diagnostic.
  - Do **not** close on early exits (K4).
- **Completion intrinsics:** `Completion`, `CompletionWithCancel`, `CompletionToken`, `CompletionTokenResolve`, `CompletionTokenCancel`, `ResolverComplete`, and `ResolverCancel`.
  - `ResolverComplete`'s "consumer gone" branch expands the glue named by the `CompletionOrphan` use through `emit_drop_site`, which deletes the last 5.6-era cleanup diagnostic.
  - The runtime releases that 5.6 already emits stay as they are.

**Gate:** the task, scheduler, completion, and `task scope` families reach zero, and the task/scheduler and cancellation 4.5 fixtures run identically.

**Step 6 notes (complete).** Every scheduler/task intrinsic emits from its records: `SchedulerCreate`/`TaskScope`/`YieldNow`/`TaskIsFinished`/`TaskCancel` from their visible arguments, `Spawn` from the call's recorded activation (concrete result, deferred-resource bundle indices, and the `Tasks` provider resolved through the recorded `resource_bindings` use, loaded through the provider pointer and lowered to the scope's scheduler field), and `Pump` from the call's two flattened arguments. A recorded product argument that lowering destructured element-by-element (a product literal) is rebuilt as legacy's packed product expression and then extracted, so the intrinsic's extract sequence matches; a whole product value arrives already unpacked by `emit_call` and passes through. Every completion intrinsic emits from `LoweredCall.runtime.completion_value_type`: record allocation and zero-init, scheduler and optional cancel-callback stores with the armed flag, the packed `(Wait T, Resolver T)` handle pair, `ResolverComplete`'s value slot and `__staple_completion_complete` call, and the consumer-gone branch whose drop is the `CompletionOrphan` use expanded through `emit_drop_site` (the last 5.6 cleanup diagnostic is gone). `FunctionEnvironment` gained `task_scopes`; `emit_with` pushes the recorded `Tasks` scope and closes it through `close_task_scopes` only on a normal exit (K4), while early exits leave it open.

The fixture exposed and fixed a real cleanup-scanner parity bug: a `let` (or destructured `let`) at module scope inside a top-level `with`/block is module-scoped in the checker but receives no module global, and legacy still owns and drops it. The scanner's `register_binding` and the artifact-use-site validator both classified every `SymbolStorage::GlobalStorage` symbol as "never owned"; they now key on the recorded `has_global` (legacy's storage map) instead, so those locals get live flags and normal scope-exit drops.

The new fixture emits and compares body for body: `scheduler`, `task_scope`, three `spawn`s, two `pump`s, `yield_now`, `Task.cancel`, `Task.is_finished`, `completion`, `completion_with_cancel`, `completion_token`, `CompletionToken.resolve/cancel`, and `Resolver.complete/cancel`, with droppable `Wait`/`Resolver`/`CompletionToken` locals dropped at scope exit. Corpus: **13968 fully emitted bodies compared (+5), 39 stubs (-5), 7 first-blocker families**. First blockers: signal read 16, until call 7, until runner artifact 5, signal binding 4, derived runner artifact 3, reaction runner artifact 3, signal notify 1. Every Step 5/6 family is absent from the first-blocker histogram; `spawn`/`pump`/`scheduler`/`task scope` remain in the reached census only because Step 7's reactive constructs stub first in bodies that also contain them. All Contract 6 gates pass: nextest and `cargo test --workspace --quiet` each pass 1310 tests, both workspace checks pass, formatting and diff checks pass, and the default and lowered-feature CLI `--emit llvm`, `--emit object`, and `run` paths produce identical stdout on the scheduler/completion program. Step 7 is next.

### Step 7: Reactive code

- **Signals.**
  - `SignalCreate` for `Global` and `LocalCell` storage (the metadata slot in the binding cell, or the module global's storage).
  - `SignalRead` tracking (`track_signal_read`).
  - `SignalNotify` on assignment, which replaces the `signal notify` diagnostic in assignment emission.
  - Signal binding items.
- **Derived.**
  - `DerivedCreate`: the evaluator closure built with the recorded `DerivedEvaluatorEnvironment` finalizer, the runner from the `ReactiveRunner` use, and the output pointer.
  - `DerivedRead` forced recompute (`force_derived_read`).
  - Derived binding items in initializers and instances.
- **Calls.**
  - `reaction`: the callback closure with its recorded `ReactiveCallbackEnvironment` finalizer, the payload with recorded slot pass modes, the runner, and `__staple_reaction_create` with the `reactive_provider`'s scope.
  - `batch`.
  - `until`: the fixed payload, the runner, and the completion.
  - `snapshot`.
  - These remove the split call families from Step 1 of 5.6.
- **Runner bodies** from `ReactiveRunnerBody`:
  - `Reaction`: load the callback and resource slots, then make the indirect call.
  - `Until`: if the completion is unresolved, call the predicate; if the `Bool` tag is alternative 0, complete.
  - `Derived`: call the evaluator and store the output.

**Gate:** every reactive family and the three runner families reach zero. The signals, reaction-resource, derived (initializer and instance), and droppable-capture-evaluator 4.5 fixtures run identically.

### Step 8: The D5 generic fixtures

- **The CLI harness gains a lowered-only expectation**, `DifferentialExpectation::LoweredOnly`. Legacy aliases or rejects these programs, so the harness runs only the lowered emitter and requires `expected_stdout`. Legacy must either fail to compile or produce different behavior, and the test records which.
- **Fixtures:**
  - a generic `coro` instantiated at two types (two distinct pairs, both correct);
  - a generic `reaction` and a generic `until` at two types;
  - a generic `derived` at two types.
  - Each prints enough to prove the second instantiation used its own pair or runner.
- **The in-process harness asserts** that each produces two distinct pair or runner artifacts. Aliased entries must appear in the census mapping's `aliased_artifacts`, not be silently skipped.

### Step 9: Runnable programs and the ratchet

- **Port the 4.5 fixture set into the corpus** as `5.8` `MustRun` entries with `expected_stdout` and focus `emits` lists. Fixtures with runtime-timing-dependent output pin only deterministic lines.
  - nested coroutines;
  - child awaits;
  - cancellation, with a droppable frame binding so the unwind drop prints;
  - `Wait` and `until` states;
  - tasks and schedulers;
  - reaction resources;
  - `until` inside a coroutine;
  - derived bindings in initializers and instances;
  - the droppable-capture evaluator.
- **Add a `coroutine_drop_order` entry.** It cancels a coroutine with two droppable frame bindings and pins the unwind order (plan order, K2). A completed sibling prints nothing for its frame bindings: the mirrored leak, with a comment naming D5.
- **Flip** `example_coroutines`, `example_signals_and_reactions`, and `example_game_loop` to `MustRun`.

### Step 10: Gate and handoff

**Gate:**

- The corpus has **zero stubs**, and `5.8` joins `COMPLETED_SUBSTAGES`. Every corpus entry is `MustRun`, `LoweredOnly`, or `CompileOnly`, and none is `MayBeBlocked`.
- Every fully emitted body matches legacy (record the count), except the D5 aliased instantiations, which the census explains. The declaration census holds.
- Every focus `emits` function is fully emitted and body-identical.
- The Contract 1 audit is clean. `codegen/lowered/` has no type-name search, no type-based drop or `Copy` decision, and no `Tasks`/`Wait`/signal type predicate. Every row of the type-query table is answered by a record.
- The legacy IR comparison after Step 1 is `same` for every shared-helper step, with `coroutines.sta` a single variant.
- The Contract 6 gates pass, and the CLI `--emit llvm`, `--emit object`, and `run` paths pass under both emitters on the coroutine and reactive examples.

**Handoff:**

- To 5.9: the lowered emitter is complete. Record the final corpus numbers, and that every former `MayBeBlocked` program runs.
- Record the mirrored behaviors for 5.11. These are the completed-coroutine leak (existing D5), plus the task-scope early-exit question (K4) as a candidate. Decide there whether it is a defect.
- Update [TYPED_LOWERING_PLAN.md](TYPED_LOWERING_PLAN.md) and the breakdown's status line.

## Ordering

```text
Step 1 → Step 2 → Step 3 → Step 4 → Step 5 → Step 6 ─┐
                     └────→ Step 7 ──────────────────┼→ Step 8 → Step 9 → Step 10
```

- Step 1 comes first because no `resume` comparison is stable without it.
- Step 2 comes before any emission that reads a fact it adds.
- Step 7 can run in parallel with Steps 4–6 after Step 3. The `until`-inside-a-coroutine fixture needs Steps 5 and 7.
- **Riskiest steps.** These are Step 4 (the state machine, the unwind, and the body emitted in another owner's arenas) and Step 5 (suspension and resume across awaits). Land them in small commits, and keep the in-process body comparison green after each.
- **Sub-plans.** None of the steps needs its own plan file, but Steps 4 and 5 should each open with a short design note in this file recording the `FunctionEnvironment` coroutine-context shape.
