# Stage 5.6 Plan: Ownership Cleanup, Finalizers, and Buffers

This is the separate plan the Stage 5.6 section of [STAGE_5_LLVM_MIGRATION_BREAKDOWN.md](STAGE_5_LLVM_MIGRATION_BREAKDOWN.md) requires. First read:

- the breakdown's Migration Contract (items 1–7) and Decisions D1–D6, especially D3 (drop glue expands inline) and D5 (defects to mirror);
- the 4.4 plan's cleanup contract ([STAGE_4_4_CLEANUP_ARTIFACTS_PLAN.md](STAGE_4_4_CLEANUP_ARTIFACTS_PLAN.md): `DropGlueBody`, `GcFinalizerPlan`, the use sites, and owned bindings);
- the post-gate review fixes in the 5.4 and 5.5 plans.

This plan's revised gate (Step 9) supersedes the **Gate** paragraph in the breakdown's 5.6 section.

Line references are against `4bca70d` and will drift; re-locate code by function name. Run the suite with `cargo nextest run --workspace`.

## Starting Point

The differential corpus (36 programs) compares 9882 fully emitted bodies and reports 424 stubs: 272 owned by 5.6, 8 by 5.7, and 144 by 5.8. The 5.6 first-blockers:

| Family | Stubs | Notes |
| --- | --- | --- |
| `buffer allocation`, `buffer capacity`, `buffer length`, `buffer push`, `buffer transfer`, `buffer freeze`, `buffer pop`, `buffer get`, `buffer clone` | 232 | the nine `Buffer*` intrinsics |
| `GC finalizer artifact` | 18 | finalizer bodies (all four subkinds) |
| `wildcard cleanup` | 16 | a `WildcardDiscard` use exists |
| `owned binding cleanup` | 5 | the body-level guard from 5.4 |
| `reference replacement` | 1 | `RefReplace` |

Each stub records only its body's **first** blocker. Behind these families are the cleanup hooks that already report a recorded drop as unsupported; they appear as first blockers as soon as the families above are emitted:

- `emit_drop_site` (five sites: `DiscardedResult`, `ReplacedValue`, `LoopBodyResult`, `IndexTemporary`, `MutateIndexTemporary`);
- `emit_call_cleanup` (`CallTemporary`, `CStringTemporary`);
- the drop intrinsic and C-string conversion sites.

**The empty program** stubs 8 bodies: six buffer intrinsics and two `reactive_scope` calls. The breakdown assigns the runnable-empty-program requirement to 5.6, which ports those two calls early. Legacy `ReactiveScope` evaluates its unit argument and makes one `__staple_reactive_scope_create` call, so this is cheap. The family they report today, `reactive call`, also covers the real 5.8 operations (reaction, batch, `until`, snapshot), so Step 1 splits it.

The legacy code this substage ports (sizes in lines):

| Legacy function | Lines |
| --- | --- |
| `compile_drop_value` + `compile_drop_value_inner` + `compile_conditional_drop` | 269 |
| `compile_conditional_cell_drop` | 60 |
| `track_symbol_ownership` + `release_moved_ownership` + `drop_owned_since` + `drop_all_owned` | 99 |
| `allocate_binding_cell` (the owned-cell and captured-cell parts) | 72 |
| `ensure_gc_finalizer` + `ensure_cell_finalizer` + `ensure_closure_finalizer` + `ensure_buffer_finalizer` | 295 |
| `compile_buffer_with_capacity` + `_metadata` + `_push` + `_get` + `_pop` + `_freeze` + `_transfer` + `_clone` + `trap_if_buffer_frozen` + `buffer_data_pointer` | 943 |
| `compile_structural_replace` (`RefReplace`) | 42 |

Legacy runs scope-exit drops in these places:

- function end;
- `return` (`drop_all_owned`);
- the propagation failure path;
- `break` and `continue`, back to the loop's `owned_before` mark (`drop_owned_since`);
- block expressions (a scope mark around the block);
- a logical's right-hand side;
- each match arm, with `restore_local_state` between arms;
- the coroutine `resume` body, which is 5.8.

## Decisions Specific to 5.6

**O1: Lowering records every cleanup decision.** Whether something drops, what drops, and in which order come only from lowering's records:

- the owner's artifact uses, each naming the `DropGlue` or `GcFinalizer` ordinal for one site;
- the `DropGlueBody`/`GcFinalizerPlan` of that artifact;
- `owned_bindings`, with each record's `storage` and `glue`, in registration order;
- the moved symbols on expression and call records.

The emitter never calls a type-based drop or `Copy` predicate. A drop position with no use record emits nothing, as E2 already established. If a position needs a drop that lowering did not record, fix the 4.4 scanner and add a use site, as the 5.4 and 5.5 reviews did.

**O2: Drop glue expands inline, from the plan (D3).** `emit_drop_glue(value, ordinal)` follows the `DropGlueBody` of one `DropGlue` artifact, recursing through each nested `PlannedArtifact`:

- `UserDrop` calls the selected `Drop` instance, then the representation glue;
- `CoroutineCleanup` calls the frame's cleanup function;
- `RuntimeRelease` makes the scheduler, wait, resolver, or completion-token runtime call;
- `CStringFree` calls the shared `build_free_c_string`;
- `Product` drops each recorded field, in recorded order;
- `Sum` switches on the tag over the recorded alternatives;
- `Distinct` drops the representation.

The block structure and SSA names must match `compile_drop_value_inner`, so that every site passes the body comparison. Conditional drops wrap the expansion: a live-flag test (`compile_conditional_drop`) or a cell-state test (`compile_conditional_cell_drop`). No `__staple_drop_glue_N` function is emitted; the census already explains inlined drop glue.

**O3: Owned-binding scopes mirror legacy exactly.** `FunctionEnvironment` gains legacy's ownership state: each owned value with its type and live-flag alloca, `owned_order`, owned cells, and scope marks. Registration follows `owned_bindings` in order, replacing legacy's `track_symbol_ownership` and the owned-cell path of `allocate_binding_cell`. Every legacy exit above drops in reverse from its mark. Match arms snapshot and restore the local state as `restore_local_state` does. The 5.4 owned-binding guard (and its `CellFinalizer` extension) is deleted at the end of Step 4.

**O4: Mirror the D5 defects.** Generic `Drop` implementations are never selected, because `DropGlueBody::UserDrop` follows the 4.4 exact-match rule. The completed-coroutine frame leak is 5.8's concern. Both are fixed in 5.11.

## Steps

Each step ends with the Contract 6 gates (`cargo nextest run --workspace` for the suite) and a commit. A step that moves legacy code into the shared layer proves legacy IR is unchanged with `scripts/compare-llvm-ir.py` against the pre-step binary, over `staple-compiler/examples/*.sta`, `examples/game_loop/main.sta`, and the Stage 5.2 ABI/coroutine probe programs, with 16 runs for `coroutines.sta`.

### Step 1: Family hygiene

- Split `reactive call` by `LoweredReactiveOperationKind`:
  - `reactive scope call` is owned by 5.6 and ported in Step 7;
  - `reaction call`, `batch call`, `until call`, and `snapshot call` are owned by 5.8.
- Split `GC finalizer artifact` into the four subkinds (`payload finalizer`, `cell finalizer`, `closure environment finalizer`, `buffer finalizer`) for progress tracking.
- Make sure every cleanup family the hooks can report is in the ownership table, owned by 5.6.

**Step 1 notes (complete).** A reactive intrinsic call resolves its `LoweredReactiveOperationKind` through the new `EmissionView::reactive_operation` accessor, and `reactive_call_family` names the split families: `reactive scope call` (5.6, 72 first-blocker stubs corpus-wide; the empty program's two calls among them), and `reaction call`, `batch call`, `until call`, and `snapshot call` (5.8, with the impossible binding/name kinds kept under the general `reactive call` family). The five reactive families replace `reactive call` in the ownership table. `artifact_family` now matches `GcFinalizerPlan` and reports `payload finalizer`, `cell finalizer`, `closure environment finalizer`, and `buffer finalizer` separately (18 stubs split 13/3/2/0). Every cleanup family the hooks can report was already in the table owned by 5.6. The harness reports 424 stubs: 344 owned by 5.6, 8 by 5.7, 72 by 5.8. The full suite passes 1298 tests.

### Step 2: Share the cleanup helpers (Contract 7)

Move the `TypedModule`-free cores into `codegen/ir.rs` and switch legacy to them:

- the four runtime-release calls;
- the coroutine frame cleanup call (load the cleanup function pointer from the frame header and call it);
- the live-flag conditional block skeleton (`compile_conditional_drop`'s branch, `drop` block, and continue block);
- the cell-state conditional skeleton (`compile_conditional_cell_drop`);
- the finalizer function entry and exit (payload pointer parameter, `finalizer.value` load, return);
- the buffer header and data operations: `buffer_data_pointer`, bounds and frozen checks (`trap_if_buffer_frozen`), header initialization and capacity growth, element load and store, and `register_gc_interior` use.

The type recursion itself stays in each emitter. Legacy recurses over types, the lowered emitter over plans, and both call the same leaf builders. The legacy IR comparison must report `same`.

**Step 2 notes (complete).** The `TypedModule`-free cleanup cores now live in `codegen/ir.rs` and legacy calls them: `build_runtime_release` (the four `RuntimeRelease` calls with their runtime function names), `build_coroutine_frame_cleanup` (load `CORO_CLEANUP_FN` and indirect-call with the frame), `begin_conditional_drop`/`end_conditional_drop` (the `drop.live`/`drop.done` skeleton and live-flag clear), `begin_conditional_cell_drop`/`end_conditional_cell_drop` (`CellDropBlocks`: state test, `cell.drop`/`cell.drop.continue`, loaded value, state clear), `add_finalizer_function`/`enter_finalizer_function`/`finish_finalizer_function` (`FinalizerBody`: the `entry` block, payload pointer parameter, and `ret void` with position restore), and the buffer operations: `buffer_data_pointer`, `trap_if_buffer_frozen`, `trap_if_buffer_capacity_overflows`, `build_buffer_allocation` (the `{prefix}.element.bytes`/select/`{prefix}.allocation.bytes`/GC-allocation/memset sequence), `build_buffer_element_pointer`/`build_buffer_element_load`/`build_buffer_element_store`, `build_buffer_length`, `build_buffer_capacity`, and `build_buffer_capacity_store`. Legacy's local `buffer_data_pointer` and `trap_if_buffer_frozen` are deleted; every legacy drop-glue, finalizer, and buffer intrinsic path now routes through the shared helpers with unchanged SSA names and instruction order. `scripts/compare-llvm-ir.py` against the Step 1 binary reports `same` for every `staple-compiler/examples` program, `game_loop/main.sta`, and the recreated ABI (generic move, non-`Copy` product, move `CString`, `mut` parameter) and coroutine/C-string probes at 4 runs; `coroutines.sta` keeps its four pre-existing frame variants and is `same` at 16 runs. The full suite passes 1298 tests.

### Step 3: Drop glue expansion and the drop sites

- Implement `emit_drop_glue` and its conditional variants (O2).
- Fill the hooks. Each one looks up its site's use record, expands the named glue at exactly that position, and does nothing when no record exists:
  - `emit_drop_site` for `DiscardedResult`, `ReplacedValue`, `LoopBodyResult`, `IndexTemporary`, and `MutateIndexTemporary`;
  - `emit_call_cleanup` (`CallTemporary` in reverse argument order, then `CStringTemporary`, as `drop_mutation_temporaries` does);
  - `WildcardDiscard` in `bind_pattern`;
  - `DropIntrinsic` (the `Drop` intrinsic) and `CStringConversion`.
- Leave `CompletionOrphan` (`ResolverComplete`) a 5.8 diagnostic; its intrinsic is 5.8.

### Step 4: Owned bindings, scopes, and moves

- Register the owned bindings (O3):
  - `OwnedStorage::Value`: the value and an `i1` live flag set true, like `track_symbol_ownership`;
  - `OwnedStorage::Cell`: the owned cell, dropped conditionally on its state.
- On a move, store `false` to the live flag of every moved owned symbol. This is the live-flag half of `release_moved_ownership`; 5.4 already landed the cell-state half.
- Add the scope marks and exit drops at every legacy exit listed above, in legacy's order. Initializers follow their own owned bindings (block locals only; module globals are never owned).
- Delete the owned-binding guard. Any body that still stubs must now name a real family.
- Add the 5.4 focus follow-up `thunk_env` to `thunk_arguments`' `emits` list. It moves an owned `CString` into an implicit thunk argument.

### Step 5: GC finalizer bodies

Emit the four `GcFinalizerPlan` bodies:

- `Payload` loads the payload and expands its glue;
- `Cell` drops the cell value conditionally on its state;
- `ClosureEnvironment` loads the environment struct and drops the recorded captures (`drops`) in recorded order, which is reverse capture order;
- `Buffer` drops each live element through the element glue (`ensure_buffer_finalizer`).

Install the finalizers that are not yet installed:

- `CellFinalizer`, at captured-cell allocation;
- `BufferAllocation`, when a buffer is created.

`RefConstruction`, `ClosureEnvironment`, and `ThunkArgumentEnvironment` were already installed in 5.4. Partial mode stops stubbing the finalizer families.

### Step 6: Buffers and `RefReplace`

Port the nine buffer intrinsics through the Step 2 helpers, keeping legacy's traps, growth policy, and SSA names:

- `BufferWithCapacity`, which installs the finalizer recorded by its `BufferAllocation` use;
- `BufferLength` and `BufferCapacity`;
- `BufferPush`, `BufferGet`, `BufferPop`, `BufferFreeze`, and `BufferTransfer`;
- `BufferClone`, which calls the element `Clone` instance named by the `BufferCloneElement` instance use for each element and installs the `BufferCloneFinalizer` on the destination.

Port `RefReplace` (`compile_structural_replace`); the previous value's drop comes from its recorded use.

### Step 7: The reactive scope call, early

Emit the `Scope` reactive operation: evaluate the unit argument, then call `__staple_reactive_scope_create` through `build_reactive_runtime_call`, exactly as legacy does. The runtime requirement set already installs `reactive.ll` when a scope exists. The other reactive operations stay 5.8 diagnostics under their split family names.

### Step 8: Runnable programs and the ratchet

After Steps 1–7, the empty program should compile strictly. Confirm that the CLI harness compiles, links, and runs it under both emitters with identical stdout and exit status, then flip the `empty` corpus entry to `DifferentialExpectation::MustRun`.

Do the same for every other `MayBeBlocked` program that now compiles strictly and behaves identically. A program that compiles strictly but behaves differently is a defect to fix, never something to leave unflipped. The `CompileOnly` census programs stay compile-only.

### Step 9: Corpus, gate, and handoff

**Corpus additions** (tagged `5.6`, each with an `emits` list naming its own functions):

| Program | Covers |
| --- | --- |
| `drop_order` | `MustRun`. A `Drop` impl that prints its payload, used across scope exit, early `return`, propagation failure, `break`/`continue`, match arms, a moved value (no drop), a replaced assignment, a discarded result, and a call temporary. Stdout records the exact drop order |
| `drop_glue_shapes` | nested droppable products and sums, a `Distinct` over a droppable, a C-string field, and a recursive nominal type through `Ref` |
| `finalizers` | `Ref` payloads, captured `mut` cells, and closure environments with droppable captures. Body-compared only: finalizer output depends on GC timing, so it is not printed |
| `buffers` | every buffer intrinsic, including frozen-buffer traps (exit status), growth, and `Clone` over a droppable element |
| `ref_replace` | `RefReplace` with a droppable payload |

Check each program's syntax against `Staple.md` and the existing fixtures. Use `extern "c" { puts: ... }` or `std.io.println` for output, whichever the empty-program work leaves fully emitted.

**Revised gate:**

- Every family owned by 5.6 has zero stubs across the corpus, including the cleanup families behind the first blockers and `reactive scope call`.
- The owned-binding guard is gone.
- `rg` finds no `concrete_needs_drop`, `concrete_is_copy`, `type_needs_drop`, or `is_copy` decision in `codegen/lowered/`, outside `LayoutContext`'s ABI use.
- Every `emits` function across the corpus, including `thunk_env`, is fully emitted and body-identical to legacy.
- The empty program and `drop_order` are `MustRun` and run identically. Every program that compiles strictly runs identically and is flipped to `MustRun`.
- The declaration census holds, and every fully emitted body matches legacy (record the count).
- The legacy IR comparison is `same` after Step 2.
- The Contract 6 gates pass.

**Handoff:**

- Record the remaining stubs: 5.7's structural method bodies and 5.8's coroutine, reactive, task, and completion work.
- Record which corpus programs became `MustRun`.
- Note for 5.8 that `CompletionOrphan` and the coroutine `resume` scope exits use the 5.6 drop machinery.
- Update [TYPED_LOWERING_PLAN.md](TYPED_LOWERING_PLAN.md) with one status note.

## Ordering

```text
Step 1 → Step 2 → Step 3 → Step 4 → Step 5 → Step 8 → Step 9
                     └──→ Step 6 ──┘   ↑
                     Step 7 ───────────┘
```

- Step 3 comes before Step 4, because scope exits expand glue.
- Step 4 comes before Step 5, because owned cells and captured cells need the finalizer installs.
- Step 6 needs Step 3 (element glue and `RefReplace`'s drop) and can run in parallel with Steps 4–5.
- Step 7 is independent and small.
- Step 8 needs everything that blocks the empty program: Steps 6 and 7, plus Steps 3–5 for whatever cleanup the standard library's eager bodies reach.
- None of the steps needs a separate plan file. Steps 3 and 4 carry the most risk, because cleanup order is behavior. Land them as small commits and keep the in-process harness green after each one; the body comparison checks drop positions per site. The 5.5 review found that the Stage 4.4 legacy comparison cannot do that. `drop_order`'s printed output is the end-to-end check.
