# Stage 5.11 Plan: Fix the Mirrored Defects

This is the plan for the Stage 5.11 section of [STAGE_5_LLVM_MIGRATION_BREAKDOWN.md](STAGE_5_LLVM_MIGRATION_BREAKDOWN.md). It includes the design note the breakdown requires before the generic `Drop` fix. First read:

- the breakdown's D5 and its 5.11 section;
- the latent-defect notes in the 4.4 and 4.5 plans;
- the 5.6, 5.8, and 5.9 review sections that added defects to the list;
- the 5.10 plan's R1 method (lowered-IR identity against a reference binary).

This plan's gate (Step 7) supersedes the **Gate** paragraph in the breakdown's 5.11 section.

Line references are against `5194cf9` and will drift; re-locate code by name. Build with `CARGO_INCREMENTAL=0`.

## Execution Notes

### Step 1 — complete

- Built the pre-fix reference at `5194cf9` (`CARGO_INCREMENTAL=0 cargo build --release -p staple --target-dir <scratch>`) and dumped the 75-entry corpus with the ignored `dump_corpus_sources` helper (`STAPLE_CORPUS_DUMP=<scratch>/corpus`). All 87 paths (11 examples, `game_loop`, and the 75 corpus entries) self-compare as single variants.
- Defect pins recorded: `thunk_arguments` (F1) and `coroutine_drop_order` (F4). No corpus or standard-library program declares a generic `Drop` implementation; the only generic ones are the 4.4 test fixtures, so no corpus entry is F5-sensitive.

### Step 2 — complete (F1)

- `ExternAdapterPlan` records `indirect_parameters`, one flag per flattened value slot, computed by `adapter_indirect_parameters` from the shared `concrete_is_copy` decision. The callable-value scanner and the family expander compute the same vector, so `check_stage_4_6`'s re-expansion rejects a disagreement.
- `emit_extern_adapter_body` loads every recorded by-pointer argument (`extern.argument`) before the unchanged native call. A whole-mutation callable is rejected; no extern binding can produce one.
- The closure route records `c_string_temporary` for an indirect call only when the callee is a statically known extern binding and its first slot is not moved, and the emitter loads the borrowed `CString` slot before releasing it. This is narrower than the plan's "caller always frees": `CString.to_string` consumes its argument (Staple.md, "C interop"), so a blanket caller free would double-free the conversion's own release (an owned `CString` passed to `CString.to_string` already aborts on the baseline). The adapter leak is fixed; an unknown callable that borrows a temporary can still leak, exactly as today.
- Fixtures: the new `extern_adapter_abi` 5.11 entry (extern callback, captured adapter in a closure, stored adapter, implicit thunk with a moved capture, and an implicit thunk with a literal temporary) and the restored `thunk_arguments` (`puts value`, pinned `thunk\n\n`).
- M1: 85 of 87 paths are `same` over four runs. The two `DIFF`s are `census_coroutines_and_runners` and `extern_values`, each differing only in the adapter body (`%extern.argument = load ptr, ptr %1`). The full workspace suite passes 1302 tests.

### Step 3 — complete (F2)

- `place_root_symbol` now follows `ProductElement` to its base, so a field write's `initialization_symbol` and `signal_notify` name the base signal. The emitter still suppresses the initialization-state writeback for a `ProductElement` target: the projection's own runtime check requires an initialized base, so a field write cannot initialize it.
- The checker accepts a field write to a possibly-uninitialized base only through a captured binding's runtime check, and the projection check traps before the store; a `mut` binding and a signal both require an initializer, so the base is always initialized when the projection executes. A field write therefore never changes initialization state.
- Fixture: `signal_field_writes` (5.11) pins `seen 0`, `seen 5`, `seen 7` over a signal product field, a nested field write, and a field write through a captured `mut` cell.
- M1: no baseline-corpus path changes for F2 (the baseline corpus has no signal field write); F1's two `DIFF`s remain. The full workspace suite passes 1302 tests.

## Starting Point

There is one emitter, and the suite passes 1302 tests. Stage 5.11 is the only part of Stage 5 that intentionally changes behavior. Five defects are queued:

| # | Defect | Found | Today |
| --- | --- | --- | --- |
| F1 | Extern-value adapter ABI mismatch | 5.6 review | A closure call passes a non-`Copy` borrowed argument by pointer, but `ExternAdapter` forwards its raw parameter to C, so `call_with puts` prints the bytes of the string pointer. The corpus's `thunk_arguments` uses `measure` instead of `puts` to avoid it. |
| F2 | Signal field writes do not notify | 5.9 review | `point.x = 5` neither notifies the signal nor initializes the base, because lowering's `initialization_symbol` yields none for `ProductElement`, mirroring legacy. A reaction over `point.x` does not re-run. |
| F3 | Task scopes stay open on early exit | 5.8 K4, a candidate | `emit_with` closes a `Tasks` scope only on a normal exit, while reactive scopes dispose on `return`/`break`/`continue` too. |
| F4 | Completed-coroutine frame-binding leak | 4.5 | A coroutine that completes normally never drops its droppable frame bindings; only the cancel unwind does (`unwind_drop`). The corpus's `coroutine_drop_order` pins the leak (`leaked` never prints). |
| F5 | Generic `Drop` implementations never selected | 4.4 | The checker (`has_drop_implementation`, `type_needs_drop`, and through them `is_copy_type`) and lowering (`drop_implementation_for`, `concrete_type_needs_drop`) require the implementation's argument to equal the type exactly, so `impl<T> Drop (Box T)` is accepted but never runs. |

What exists:

- **Drop machinery.** The emitter's drop machinery (`emit_drop_glue`, the conditional cell drop, owned scopes) and the pair plan's `unwind_drop` already handle every drop a fix needs to emit.
- **`Copy` predicate.** The checker and lowering already share one `is_copy_type` (`typecheck.rs`). Its `Copy` decision consults `has_drop_implementation`, so F5's `Copy` consequence lands in one function. The two needs-drop predicates are separate copies.
- **Coherence.** At declaration, the checker rejects any two implementations of a trait whose headers could apply to the same type. This is `implementation_headers_overlap`, and it is bound-aware. Overlapping `Drop` implementations therefore cannot exist today.
- **Trait selection.** Ordinary trait selection already unifies headers and discharges bounds: `dispatch_matching_implementations` in the checker, `resolve_trait_evidence`/`select_concrete_trait_method_with_kind` in lowering. The latter is already used with a non-default instance-edge kind (`CloneMethod`), and `LoweredInstanceDependencyKind::DropMethod` exists.
- **Generic `Drop` uses.** The only generic `Drop` implementations in the tree are test fixtures: the 4.4 `Box` fixture, `impl<T where Copy T> Drop (Box T)`, in `cleanup_artifacts.rs` and `graph_validation.rs`.

## Decisions Specific to 5.11

**M1: One commit per defect, each proving its blast radius.**

- Before each fix, build the pre-fix tree as a release reference, and dump the corpus with `dump_corpus_sources`.
- After the fix, compare the 87 paths (11 examples, `game_loop`, 75 corpus programs) with `scripts/compare-llvm-ir.py`.
- Every `DIFF` must be a program that exercises the defect. The step notes list each one with the reason, and every other path must stay `same`.
- Corpus programs that pinned a defect have their expectations changed deliberately, in the same commit, with a comment naming the defect (`coroutine_drop_order`, `thunk_arguments`).

**M2: Each fix adds runnable fixtures that print the corrected behavior.** They become corpus `MustRun` entries tagged `5.11`, with pinned stdout, so the corrected behavior is permanently guarded.

**M3: F3 is a defect, and it is fixed.** A `Tasks` scope abandoned by `return`, `break`, or `continue` leaves its spawned children queued against a dead scope. That is inconsistent with reactive scopes and observable. Close task scopes on the same exits that dispose reactive scopes, in the same order relative to owned drops. Reactive disposal comes before owned drops; task-scope closing goes immediately after reactive disposal. Record the ordering in the step notes.

**M4: The F5 semantics are fixed by the design note below before any F5 code lands.**

## Design Note: Generic `Drop` Selection (F5)

**Rule.** A `Drop` implementation applies to a concrete type `C` exactly when its header unifies with `C` and its bounds hold under that unification. This is the rule every other trait already uses: `Drop` stops being a special exact-match lookup.

**Uniqueness, with no precedence.** Coherence already rejects overlapping `Drop` implementations at declaration, bound-aware. This was confirmed while writing this note: both `impl<T> Drop (Box T)` and `impl<T where Copy T> Drop (Box T)` beside `impl Drop (Box I32)` are rejected. For example, `impl Drop (Box I32)` beside `impl<T> Drop (Box T)` is a "duplicate trait implementation" error. At most one implementation applies to any concrete type, so selection needs no precedence rule. Step 6 adds a test asserting that this overlap is rejected, so the property cannot silently regress.

**Allowed bounds.** A `Drop` implementation's bounds may constrain only the implementation's own type parameters (for example `impl<T where Copy T> Drop (Box T)`), never a type that contains the header itself (`where Copy (Box T)`). The checker rejects a violating bound at declaration with a diagnostic.

- **Reason:** `is_copy_type` asks whether a `Drop` implementation applies, and discharging a bound may ask `is_copy_type` again.
- **Termination:** restricting bounds to the parameters makes every recursive question about a strict subterm of the type, so the recursion terminates. The existing 4.4 fixture's `where Copy T` is allowed.

**Concrete types versus types with parameters.** Ownership is checked once, on the generic template, so a template's view must hold for every instantiation.

- **Concrete types** (no type parameters): an implementation applies only if its bounds discharge. So with `impl<T where Copy T> Drop (Box T)`, `Box I32` has a user drop and `Box CString` does not (it still needs drop structurally, through `CString`).
- **Types that still contain parameters** (template checking): a `Drop` implementation *may* apply if its header unifies, ignoring bounds. A type that may have one is treated as not `Copy`.

This is sound:

- **Templates are never too permissive.** A template never copies a value that some instantiation would drop.
- **Instantiations agree on what drops.** Instances, checked concretely, drop exactly the types whose implementation applies.
- **`Copy` disagreement is harmless.** At worst, a concrete instantiation is `Copy` where the template enforced moves (moves of a `Copy` value are copies).

**One predicate on both sides.** Replace the checker's `has_drop_implementation`/`type_needs_drop` and lowering's `drop_implementation_for`/`concrete_type_needs_drop` with one shared implementation-matching function, as `is_copy_type` is already shared. It is parameterized by the bound-discharge callback each side already supplies. Agreement is then by construction. An agreement test still sweeps the fixtures, extending `drop_glue_plans_agree_with_the_typed_module` and `layout_context_agrees_with_checker_predicates`. It checks that `type_needs_drop`/`concrete_needs_drop` and `is_copy_type`/`concrete_is_copy` agree on every concrete type in each fixture's catalog.

**Lowering selects through trait resolution.** `DropGlueBody::UserDrop` takes its method from `select_concrete_trait_method_with_kind(.., DropMethod)`, which yields an instance request carrying the implementation's substitutions. The drop method is then an ordinary specialized instance, with a `DropMethod` edge, catalog naming (D2), and re-expansion validation. A generic `Drop` body instantiated at two types gives two instances.

**Ownership inside drop methods.** `is_drop_method` and the owned-binding rule that drop methods do not own their own parameter must recognize a specialized instance of a generic drop method, not only the template. Step 6 checks this, and adds a fixture whose generic drop body would double-drop if it owned its parameter.

**Language consequence (intended).** A type with an applicable `Drop` implementation is not `Copy`. Programs that copied a value of a generically-droppable type now get move errors where they previously double-copied a value whose drop never ran. Only the 4.4 test fixtures declare generic `Drop` today, and the step notes record any other program whose diagnostics change. Staple.md's `Drop` section is updated to state the rule and the bound restriction.

**`drop_method_for`.** The `#[cfg(test)]` checker query from 5.10 is replaced in its five lowering tests by the shared predicate, then deleted.

## Steps

Each step ends with the suite (`cargo nextest run --workspace`), the M1 comparison, formatting, `git diff --check`, and one commit.

### Step 1: Baseline

- Build the release reference at `5194cf9` in a scratch target directory, dump the corpus, and confirm the 87 paths self-compare as single variants.
- Record which corpus entries pin a defect and will change: `coroutine_drop_order` (F4), `thunk_arguments` (F1), and any F5-sensitive entry found by searching for `Drop` implementations.

### Step 2: F1, the extern-value adapter ABI

- **Adapter parameters.** Give the `ExternAdapter` plan, and so the adapter function, the closure ABI's parameter shapes: a by-pointer parameter wherever `indirect_parameter_mask` marks one for the adapter's callable type. The adapter loads each such argument before the native call. Lowering records the per-parameter pass mode on the plan, and re-expansion validates it.
- **The native extern call is unchanged.**
- **C-string temporary ownership: the caller always frees, and the adapter never does.**
  - A closure call whose `CString` argument is a temporary records a `CStringTemporary` use, just as the direct extern route does, and the call's cleanup frees it.
  - This makes the closure route stop leaking without making the adapter an owner.
- **Fixtures:** an extern `CString -> I32` passed as a callback, captured in a closure, and captured in an implicit thunk. Each prints the right text. Restore `thunk_arguments` to `evaluate { puts value }` and pin its output.

### Step 3: F2, signal field writes

- **Root symbol.** A field write's place resolves to its base's root symbol for notification and initialization: `initialization_symbol` follows `ProductElement` to the base again, for these purposes only.
- **Notification.** The write stores the field, then notifies the base's signal, recording `SignalNotify` on the assignment as for a whole-value write.
- **Initialization.** First establish, with a correctly written probe, whether the checker accepts a field write to an uninitialized base. The planning probe used the wrong declaration syntax and did not parse.
  - If the checker rejects it, a field write never changes the initialization state; document that.
  - If it accepts it, decide and document whether a field write initializes the base. It should not, unless every field is written.
- **Fixtures:** a reaction over a signal product field printing `seen 0`, `seen 5`, `seen 7`; a nested-field write; a field write through a captured `mut` cell.

### Step 4: F3, task scopes on early exit (M3)

- **Exits.** `return`, `break`, and `continue` close every task scope opened since their target: the whole function for `return`, the loop's mark for `break`/`continue`, through a new `LoopContext::tasks_before`. They do so right after reactive-scope disposal and before owned drops.
- **Normal exit.** The `with Tasks` normal exit is unchanged.
- **Fixtures:**
  - a function that spawns a long-running task inside `with Tasks` and returns early: the child is cancelled, `Task.is_finished` reports `True`, and the child's trailing output never prints;
  - the same with `break` out of a loop.

### Step 5: F4, completed coroutines drop their frame bindings

- **Plan.**
  - Add completion drops to `CoroutineFramePlan`: each droppable frame binding's glue, in plan order, bound like `unwind_drop`. Reuse the same `PlannedArtifact`; a separate field is not needed if `unwind_drop` already names the glue, so state the choice.
  - Extend `expand_coroutine_codes`, `check_stage_4_5`'s re-expansion, `visit_types`/`visit_callees`, and the catalog snapshot.
- **Emission.** On the body's normal-return path in `resume`, after `drop_all_owned` and before the result is published, run a conditional cell drop of each frame binding in plan order. The cell state ensures that a moved-out or never-initialized binding is not dropped.
  - A cancelled coroutine still drops exactly once, through the unwind.
  - A completed coroutine's later `cleanup` must not drop again. The frame is `DONE`, and the unwind and completion paths are mutually exclusive.
- **Fixtures:**
  - a printing `Drop` held in a coroutine local that completes normally;
  - a binding moved out before completion (no drop);
  - a binding in a branch that never ran (no drop);
  - a cancelled coroutine (exactly one drop);
  - nested and child-awaited coroutines.
  
  Update `coroutine_drop_order`: the completed sibling's `leaked` now prints.

### Step 6: F5, generic `Drop` selection (design note)

Land in this order, as separate commits if large:

1. **The shared drop-implementation predicate and the bound restriction.**
   - Add the shared matching function and the declaration diagnostic for bounds that mention the header.
   - Switch the checker's `type_needs_drop` and `is_copy_type` and lowering's `concrete_needs_drop` to it, with the concrete-versus-template rule above.
   - Before switching behavior, an agreement test sweeps every fixture.
2. **Lowering selection.** `UserDrop` takes its method through `select_concrete_trait_method_with_kind(.., DropMethod)`. Generic drop methods become specialized instances with catalog names. Re-expansion and the catalog census cover them.
3. **Ownership inside generic drop instances.** `is_drop_method` and the owned-binding rule must recognize specialized instances.
4. **Cleanup.** Replace `drop_method_for` in its five tests, then delete it. Update Staple.md.

**Fixtures:**

- a generic `Drop` on a nominal wrapper at two instantiations: both drop, each through its own instance;
- `impl<T where Copy T> Drop (Box T)` at `Box I32` (user drop prints) and at `Box CString` (no user drop; the `CString` is still freed);
- nested generic droppables inside products and sums;
- a value moved out of a generic droppable;
- a closure and a coroutine capturing one;
- the overlap rejection (`impl Drop (Box I32)` beside `impl<T> Drop (Box T)`);
- the bound-restriction diagnostic;
- a template that tries to copy a `Box T` and gets a move error.

Replace the 4.4 `Box` fixture's "not selected" assertion with "selected".

### Step 7: Gate and close Stage 5

**Gate:**

- **Fixtures.** Every fixture from Steps 2–6 prints its corrected output and is a pinned `MustRun` corpus entry.
- **Containment.** Each fix's M1 comparison shows `DIFF` only in programs that exercise its defect, each listed with the reason. All other paths are `same`.
- **Agreement.** The checker/lowering drop and `Copy` agreement test passes over the fixture sweep.
- **Suite.** `cargo nextest run --workspace` passes, the build is warning-free, and formatting and `git diff --check` pass.
- **CLI.** The `--emit llvm`, `--emit object`, and `run` paths pass on the examples with the worktree standard library.

**Close-out:**

- Update [TYPED_LOWERING_PLAN.md](TYPED_LOWERING_PLAN.md), the breakdown (D5, the 5.11 section, and the status line), and the 4.4/4.5 plans' latent-defect notes to say where each defect was fixed.
- **Mark Stage 5 complete** in the main plan and name Stage 6 as next. Stage 6's handoff (the unused `TypedModule` queries from 5.10) is unchanged, apart from `drop_method_for`, which Step 6 deletes.

## Ordering

```text
Step 1 → Step 2 (F1) → Step 3 (F2) → Step 4 (F3) → Step 5 (F4) → Step 6 (F5) → Step 7
```

- **Independent fixes in rising order of risk.** F1–F3 are independent, small, and local to lowering or emission; they go first, each with its own commit and M1 comparison.
- **F4 comes before F5.** F5 changes which types need drop, and F4's completion drops should be validated against today's drop set first.
- **F5 is last.** It is the only fix that changes language semantics: `Copy` reclassification and new move errors. Its design note is fixed before code lands (M4).
- **Highest risk.** This is F5's template-versus-concrete `Copy` rule. A mistake there is a soundness bug, a double drop, rather than a missing feature. That is why the design note specifies the conservative template rule and an agreement sweep that runs before the switch.
