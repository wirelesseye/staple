# Stage 5.4 Plan: Calls, Callable Values, Closures, and Resources

This is the separate plan the Stage 5.4 section of [STAGE_5_LLVM_MIGRATION_BREAKDOWN.md](STAGE_5_LLVM_MIGRATION_BREAKDOWN.md) requires. Read the breakdown's Migration Contract (items 1–7), Decisions D1–D6, the [Stage 5.3 plan](STAGE_5_3_EMITTER_SKELETON_PLAN.md), and the breakdown's Stage 5.3 **Post-gate review fixes** first. This plan's revised gate (Step 10) supersedes the **Gate** paragraph in the breakdown's 5.4 section.

Line references are against `71be158` and will drift; re-locate code by function name.

**Status:** Steps 1–2 landed.

Step 1: the mixed `call resources, mutation, or cleanup` bucket is split into `call resources`, `call initialization check`, `call mutation argument`, `call moved ownership`, `call C-string temporary`, and `reactive call` (the old 722 split exactly as 617/62/2/2/0/39). `codegen/differential.rs` now owns the `(family, owning substage)` table, the in-process harness fails on an unclassified family and prints stub totals per owning substage, and `DifferentialProgram::emits` plus the focus assertion are in place for the Step 10 corpus entries. A body-level owned-binding guard makes any body that owns a droppable binding fail with `owned binding cleanup` (5.6) before its root block is emitted. Baseline after Step 1: 2278 fully emitted bodies compared (unchanged), 3236 stubs across 40 families, now 1912 (5.4), 1167 (5.5), 43 (5.6), 7 (5.7), 107 (5.8). The guard newly stubs four bodies that previously reported `coercion or move`; no previously fully emitted body was affected.

Step 2: the call-family helpers are shared `Backend` operations in `codegen/{abi,ir}.rs` and legacy calls them: `build_closure_value`; the capture layout/allocation core (`capture_environment_type`, `insert_capture`, `allocate_capture_environment`); `build_product_value`; the `build_numeric_to_string`, `build_string_add`, and `build_bool_value` cores; the new `build_float_binary`/`build_float_compare`; `build_initialization_check`; `build_index_pointer`/`build_slice_length`/`build_slice_get_ref`; `build_argument_temporary` for indirect/mutation materialization; and the variadic acceptance rule `variadic_argument_count_matches`. The legacy IR comparison reports `same` for every `staple-compiler/examples/*.sta` program plus an ABI probe (generic `move`, non-`Copy` product, `move CString`, `mut` parameter) and a coroutine/C-string probe at 4 runs, and `coroutines.sta` the same four pre-existing `HashMap`-ordered frame variants at 16 runs.

Step 3: string literals go through the shared `build_string_literal`; a checked name runs the shared initialization check before a cell or module-global read (cell reads mirror legacy `compile_symbol_value`'s state/value slots); `mut`/captured locals allocate a plain `{value, state}` binding cell (GC-allocated exactly when some function captures it) with the state stores; and `LoweredExpression::moved_symbols` releases moved cell ownership by storing state 0, with `LoweredCall::moves` no longer a separate diagnostic (the markers only feed Step 4's pass modes). The Step 1 guard now also stops a body whose artifact uses contain a `CellFinalizer`, so a captured droppable cell cannot emit without its 5.6 finalizer. Results: 2327 fully emitted bodies compared (+49), 3187 stubs; `string`, `call moved ownership`, `binding cell read`, and `mutable or moved pattern binding` reach zero; `coercion or move` drops from 62 to 19 (coercions only), and progress exposes `nominal pattern binding` (40), `product` (22), `buffer freeze` (19), `constructor call` (4), and `reactive or cell binding` (3) as the next blockers. The Stage 5.3 stub fixture moved from string literals to string templates, which are still a 5.5 diagnostic.

Step 4: `emit_call` assembles every argument from its recorded `pass_mode` — `Value` in its slot; `BorrowedPointer`/`MutablePlace` through `emit_place_pointer` (symbol, captured cell, resource place) or a `borrow.temporary`/`mutation.temporary` alloca; `MaterializedTemporary` as a `borrow.temporary` copy. Call steps run in recorded order, including `ProductElement`, `ProductSpread`/`NamedProductSpread` (one operand evaluation, mapped element extraction) and `Default`; an expression of `Never` type now emits legacy's `unreachable`. `emit_call_cleanup` (the final 5.6 hook) runs after the call in legacy's order — mutation temporaries in reverse argument order, then a non-extern C-string temporary — and diagnoses `call argument cleanup` until 5.6 fills it. Callee initialization checks run before arguments, and native extern and intrinsic routes evaluate by value exactly like legacy's `compile_arguments`. A recorded `writeback` is diagnosed rather than skipped (lowering never sets the flag today). Results: 2401 fully emitted bodies compared (+74), 3113 stubs; `call mutation argument`, `call argument pass mode`, `call moved ownership`, `variadic extern call`, and `materialized argument` reach zero; progress exposes `trait call` (431), `product` (270), `callable adapter or initialization check` (110), the buffer families (5.6), `representation place` (5.5), `constructor call` (6), and `completion` (5.8). The 5.5 place kinds joined the ownership table.

Step 5: hidden effect-row arguments are emitted in row order through the provider each `LoweredResourceUse` records (`Value` passes the provider value, loading `resource.copy` through an indirect provider; a borrow requirement passes the provider pointer), and they are placed after the visible arguments with the callee parts extracted between them, matching legacy. `resource` reads load `resource.borrow` through an indirect provider. `with` evaluates its provider value, reuses the recorded source place (`Place`) or a `resource.provider` alloca (`Materialized`), binds it under `LoweredWith::provider`, disposes a reactive scope on a normal exit, and diagnoses a `Tasks` scope (5.8). Lowering now records the reused `LoweredWith::place` (a new field, cloned by the instance-body cloner and walked by reachability validation) instead of making emission re-derive it. Results: 2402 fully emitted bodies compared, 3112 stubs; `call resources` and `call initialization check` reach zero, while most resource-using bodies still stop at a later family (closures, patterns).

Step 6: trait-implementation calls emit the bound instance directly with a null environment (`trait.call`), structural-method calls call the declared structural artifact the same way, `Value` constructors rebuild the representation through the shared `build_product_value`, and a `ManagedRef` construction GC-allocates the payload and sets the declared finalizer from the call's `RefConstruction` artifact use. Implicit thunk arguments build the thunk instance's closure over the current scope (capture environment plus code); a thunk whose legacy closure would install a GC finalizer is diagnosed until 5.6, because no closure-environment use is recorded for thunk arguments. Two pre-existing gaps this step surfaced are fixed: a mutable-storage name read now runs the same initialization check legacy does (`requires_initialization_check || mutable`), and a nominal destructuring parameter (`Ref left`) is diagnosed as `parameter destructuring` (5.5) before a wrongly typed binding can be emitted. Results: 2817 fully emitted bodies compared (+415), 2697 stubs; `trait call` and `constructor call` reach zero, and the remaining 5.4 stubs are the intrinsics (Step 9) and the closure/adapter work (Steps 7–8).

Step 7: a `Fresh` callable value builds its capture environment from the closure plan's ordered captures (shared cell/borrowed captures store a pointer from the scope, by-value captures store their value, using the shared layout and allocation) and installs the declared finalizer from the value's `ClosureEnvironment` artifact use; `Stored` loads the existing closure from the binding's local, cell, or module storage; `Current` and `None` keep their 5.3 paths; and the `Constructor`/`External` adapters build their declared artifact's closure with a null environment. A callable value's initialization check runs the shared check first, and intrinsic callable values stay a diagnostic (legacy has no path for them). Results: 3054 fully emitted bodies compared (+237), 2460 stubs; `fresh closure environment`, `callable adapter or initialization check`, and `stored closure` reach zero, leaving the Step 8 adapter artifact bodies and the Step 9 intrinsics as the only 5.4 first-blockers.

Step 8: constructor adapter bodies rebuild the closure parameters with the shared `build_product_value` and return it, or GC-allocate the `ManagedRef` payload and set the planned payload finalizer; extern adapter bodies forward the closure parameters to the foreign symbol under the native signature. Partial mode no longer stubs either family, and both are body-compared like any other artifact. Results: 3062 fully emitted bodies compared (+8), 2452 stubs across 40 families; `constructor adapter artifact` and `extern adapter artifact` reach zero. The only remaining 5.4 first-blockers are the Step 9 numeric/string/slice intrinsics (380 integer comparisons, 152 float arithmetic, 76 float comparisons, 19 each for slice length, slice reference, and string addition).

## Starting Point

The lowered emitter (`codegen/lowered/mod.rs`, about 2k lines) declares every catalog entry, emits initializers and `main`, and already emits:

- direct, indirect, and native extern calls with value-passed arguments, and the extern C-string temporary;
- the integer arithmetic and C-string conversion intrinsics, through shared `Backend` builders;
- C-string literals, simple bindings and patterns, and a never-returning loop;
- `Stored` callable values, loaded from the function binding's global, and name reads in legacy's order.

Every other construct returns a diagnostic, which strict mode reports and partial mode (`compile_lowered_partial`) turns into a trap stub. The Stage 5.3 differential harness compares 2278 fully emitted bodies with legacy. The 19-program corpus reports 3236 stubs across 35 construct families. Each stub records only the **first** construct its body could not emit, so unlocking a family exposes whatever came next in those bodies. Progress is therefore measured per family, not by the total stub count.

Families at `71be158` that this substage owns, fully or in part (corpus-wide counts):

| Family (diagnostic text) | Stubs | 5.4 owns |
| --- | --- | --- |
| `call resources, mutation, or cleanup` | 722 | a mixed bucket: hidden resources, initialization checks, mutation arguments, and part of `moves`. The reactive and cleanup parts belong to other substages; Step 1 splits it |
| `integer comparison` | 380 | all |
| `fresh closure environment` | 191 | all |
| `trait call` | 182 | all (the call; structural method bodies are 5.7) |
| `float arithmetic`, `float comparison` | 228 | all |
| `variadic extern call` | 58 | all |
| `call argument pass mode` | 49 | all |
| `callable adapter or initialization check` | 47 | all |
| `string` (literal) | 29 | all |
| `slice length`, `slice reference`, `string addition` | 57 | all |
| `constructor adapter artifact`, `extern adapter artifact` | 8 | all (the artifact bodies) |
| `checked or reactive name` | 9 | the checked-name half; reactive names are 5.8 |
| `coercion or move` | 66 | the `move` half; coercions are 5.5 |

Families this substage does **not** own: `match`, `parameter destructuring`, other patterns, places beyond symbol roots, `assignment`, `access`, `product`, `index`, `string template`, and coercions (5.5); drop glue, owned-binding cleanup, finalizer bodies, and buffer intrinsics (5.6); structural method bodies (5.7); and coroutine, reactive, task, and completion work (5.8).

Two gaps found while planning, which Step 1 closes:

- **Owned droppable bindings are not diagnosed.** The emitter never reads `owned_bindings`. A body that owns a binding with drop glue is emitted without its scope-exit drop, and only the body comparison would notice. 5.4 unlocks many more bodies, so the guard must be explicit.
- **The 5.3 gate text in the breakdown's 5.4 section asks for runnable programs.** No program can run before about 5.6 (5.3 finding F1: the standard library's eager bodies need `match` and cleanup). The revised gate here uses the harness instead.

## Contract Rules Specific to 5.4

These restate Contract 1 for this substage's decisions. Each one is a place where legacy computes something that lowering already records.

- **Arguments.** Read `LoweredCallArgument::pass_mode`, `slot`, `temporary`, `writeback`, and `drops_after_call`. Never derive a pass mode from `concrete_is_copy` or the parameter type.
- **Hidden resources.** Read `LoweredCall::resource_bindings` → `LoweredResourceUse::provider` → `FunctionEnvironment::resources[provider]`. Never choose a provider by type.
- **Call targets.** Read the owner's binding table: `LoweredBindingSite::Call` / `CallableValue` / `CallArgumentThunk` → `LoweredBoundTarget`. Never select a trait implementation, and never resolve an instance.
- **Closure layout.** Read `LoweredClosureConstruction` (capture order, `LoweredCaptureAccess`, `owns_value`, `drops_value`) and the environment's `ClosureEnvironment` artifact use for the finalizer.
- **Adapters.** Read `LoweredCallableAdapter`, and the `ConstructorAdapter`/`ExternAdapter` artifact plans for the adapter bodies.
- **Backend-local.** LLVM calling conventions stay in the backend: indirect-parameter materialization, C variadic argument promotion, closure `{code, environment}` layout, and GC allocation.

## Steps

Each step ends with the Contract 6 gates and a commit. A step that moves legacy code into the shared layer (Contract 7) also proves legacy IR is unchanged with `scripts/compare-llvm-ir.py`, comparing against the pre-step binary over `staple-compiler/examples/*.sta` and the Stage 5.2 ABI/coroutine probe programs (recreate them from the 5.2 section if the scratchpad is gone). Use at least 16 runs for `coroutines.sta`.

### Step 1: Family ownership, focus assertions, and the ownership guard

- **Split the mixed bucket.** Replace `call resources, mutation, or cleanup` with one diagnostic per fact: `call resources`, `call initialization check`, `call mutation argument`, `call moved ownership`, `call C-string temporary`, and `reactive call`. Re-run the harness and record the new counts.
- **Family ownership table.** In `codegen/differential.rs`, add a `(family, owning substage)` table covering every diagnostic text the lowered emitter can produce. The in-process harness fails on a family that is not in the table, and prints stub totals per owning substage. It becomes the progress report for 5.4–5.8.
- **Focus assertions.** Add `DifferentialProgram::emits: &'static [&'static str]`, a list of template names from the program's own module (matched against `LoweredFunction::name`). Every instance of each listed template must be fully emitted (not stubbed) and body-identical to legacy. This is the per-feature gate that does not need a runnable program.
- **Owned-binding guard.** A body whose `owned_bindings` contains a record with `glue: Some(..)` or `storage: OwnedStorage::Cell` fails with `owned binding cleanup` (owner 5.6) before its root block is emitted. Apply the same rule to initializer owned bindings. Record any body that newly fails this way; if it was previously counted as fully emitted, the body comparison should also have flagged it.

### Step 2: Move the call-family helpers into the shared layer

Apply Contract 7 before writing any new emission. Move these legacy operations, or their `TypedModule`-free cores, into `codegen/ir.rs` (or `abi.rs` where they are ABI) and switch legacy to them:

- `build_closure_with_code`/`build_closure_value` (the `{code, environment}` value);
- the capture-environment field layout and allocation core of `build_capture_environment`/`compile_capture_type`;
- `build_product_value` (constructor representation);
- the `compile_numeric_to_string`, `compile_string_add`, and `compile_bool` cores;
- the integer compare builder (already shared), plus new float binary and compare builders;
- `build_initialization_check`/`check_symbol_initialization` (the state load, compare, and trap);
- the indirect-argument materialization in `compile_indirect_argument_pointer` (alloca, store, pointer);
- the C variadic argument promotion inside `compile_arguments(.., is_var_arg)`;
- the slice length and get-ref builders used by `SliceLength`/`SliceGetRef`.

The legacy IR comparison must report `same` for every program.

### Step 3: Names, literals, and binding cells

- **String literals** through the shared `build_string_value`.
- **Checked names.** A `LoweredName` with `requires_initialization_check` runs the shared initialization check before the read.
- **Binding cells** for `mut` locals and captured bindings: allocation (legacy `allocate_binding_cell`), reads, stores, and the local initialization-state stores (`store_local_initialization_state`). 5.4 owns cells because mutation arguments (Step 4) and cell captures (Step 7) need them. A cell whose value needs drop is still stopped by the Step 1 guard (5.6).
- **Moved ownership** (`LoweredExpression::moved_symbols` and `LoweredCall::moves`). For a symbol with mutable storage, store initialization state 0, as legacy `release_moved_ownership` does. The live-flag store for an owned droppable symbol is 5.6. The Step 1 guard already stops those bodies, so no move is silently dropped.

### Step 4: Call arguments

Port `compile_effect_arguments`/`compile_arguments`/`compile_mutation_argument_pointer`/`compile_indirect_argument_pointer`/`drop_mutation_temporaries` against `LoweredCallArgument` and `LoweredCallStep`:

- **Pass modes:**
  - `Value`, in its ABI slot;
  - `BorrowedPointer`, the address of the caller's value or a materialized copy, following `temporary`;
  - `MutablePlace`, the place pointer;
  - `MaterializedTemporary`, an alloca holding the value.

  Write back after the call where `writeback` is set.
- **Place pointers.** Add `emit_place_pointer(owner, PlaceId, environment)` for symbol-rooted places: a local, a binding cell, a parameter pointer, a module global, a captured cell, or a resource place. Every other place kind returns a diagnostic owned by 5.5, which extends this function; do not add a second place emitter.
- **Call steps.** Evaluate `ProductElement`, `ProductSpread`, `NamedProductSpread` (element extraction by the recorded mappings), and `Default` (the default expression, in slot order) exactly in step order.
- **`drops_after_call` and C-string temporaries on non-extern routes** go through one `emit_call_cleanup` hook in legacy's order (reverse argument order, after the call). Until 5.6 lands, the hook diagnoses `call argument cleanup` (owner 5.6). Its signature and placement are final, so 5.6 fills in only the body.
- **Initialization checks.** `LoweredCall::initialization_checks` run the shared check for each symbol before the call.
- **Variadic native externs** use the shared promotion helper, which removes the 5.3 diagnostic.

### Step 5: Resources

- **Hidden resource arguments.** Emit `resource_bindings` in effect-row order after the visible arguments. Each value comes from `FunctionEnvironment::resources[use.provider]`, passed as `LoweredResourceUse::pass_mode` and `indirect` specify (legacy `compile_resource_arguments`).
- **`Resource` expressions** read their provider's value (a load when `indirect` is set).
- **`With` scopes.** Evaluate the provider value, then give the provider its storage according to `LoweredProviderStorage::Place` (the source place pointer) or `Materialized` (an alloca). Bind it under `LoweredWith::provider` for the body, then remove it on exit.
  - Scope exit `Ordinary` does nothing more.
  - `Reactive` disposes the scope through the shared reactive runtime call, as the entry harness does.
  - `Tasks` returns a diagnostic owned by 5.8.
- **Resource places** (resource assignment targets) resolve through `emit_place_pointer`. The assignment item itself is 5.5.

### Step 6: Call targets

- **Trait implementation calls.** The `Call` binding is an `Instance`; emit it exactly like a direct call. `TraitEvidence` is not read at emission time.
- **Structural method calls.** The binding is an `Artifact`; call the declared structural function (its body stays a stub until 5.7).
- **Constructor calls:**
  - a `Value` construction builds the representation with the shared `build_product_value`;
  - a `ManagedRef` construction GC-allocates the payload and sets the finalizer from the call's `RefConstruction` artifact use (the finalizer body is 5.6; its declaration exists).

  The `String` constructor keeps its 5.3 path.
- **Implicit thunk arguments.** The `CallArgumentThunk` binding names the thunk instance; build its closure over the current environment, as legacy `compile_adapted_call_argument` does.
- **Reactive calls** (`LoweredCall::reactive`) keep a diagnostic owned by 5.8.

### Step 7: Callable values and closure construction

- **`Fresh` environments.** Build the capture environment from `LoweredClosureConstruction::captures` in order, one field per capture:
  - a value copy or move, following `owns_value`;
  - a pointer to a binding cell;
  - a parameter pointer, for borrowed or mutated-parameter captures;
  - an initialization-state pointer;
  - a derived-binding cell.

  Use the shared layout and allocation (Step 2) and the `capture_stores_pointer`/`capture_is_parameter_pointer` rules 5.3 fixed. Set the environment's finalizer from the `ClosureEnvironment` artifact use; the finalizer body is 5.6.
- **`None` and `Current` environments** keep their 5.3 paths. `Stored` also covers a local (non-global) function binding: load from the binding's local or cell, following `compile_symbol_value`.
- **Adapters.**
  - `Constructor` and `External` values call their adapter artifact (`ConstructorAdapter`/`ExternAdapterValue` use).
  - `Curried`, `NestedClosure`, and `ImplicitThunk` follow legacy `compile_symbol_value`/`compile_adapted_call_argument` for code and environment selection.
- **Initialization checks.** `requires_initialization_check` on a callable value runs the shared check.
- **Intrinsic callable values** stay a diagnostic unless legacy supports them; check `compile_symbol_value`.

### Step 8: Adapter artifact bodies

- **`ConstructorAdapter`.** The body unpacks the closure-ABI parameters into the flattened slots of `ConstructorAdapterPlan` and builds the value (`Value`) or the managed reference with its payload finalizer (`ManagedRef`), exactly as legacy `ensure_constructor_adapter` does.
- **`ExternAdapter`.** The body forwards the closure-ABI parameters to the foreign symbol with the extern's native signature. The declared arity comes from `ExternDeclaration`. Follow the adapter legacy builds in `declare_external_functions`.
- Partial mode stops stubbing these two families, so the census checks their bodies like any other.

### Step 9: Numeric, string, and slice intrinsics

Port `IntegerCompare`, `FloatBinary`, `FloatCompare`, `ToString` (`snprintf` through the runtime requirement), `StringAdd`, `SliceLength`, and `SliceGetRef` through the shared builders (Step 2), with legacy's SSA names. Buffers, `RefReplace`, and `Drop` stay 5.6 diagnostics. The coroutine, reactive, task, and completion intrinsics stay 5.8 diagnostics.

### Step 10: Corpus, gate, and handoff

**Corpus additions** (tagged `5.4`, each with an `emits` list naming its own functions and marked `MayBeBlocked`):

| Program | Covers |
| --- | --- |
| `calls_generic` | a generic function called at two instantiations (`I32` and `String`) |
| `calls_curried_defaults` | curried and juxtaposed calls with default arguments, spreads, and designated elements |
| `calls_mutation` | a `mut` parameter, a `mut` local passed by mutation, and a borrowed non-`Copy` argument |
| `closures_captures` | value, `mut`-cell, and borrowed captures inside a generic function |
| `extern_values` | an extern call and an extern used as a closure value, using a libc symbol so the program links |
| `constructors` | constructor calls and values, including `Ref` |
| `resources_with` | a `with`-provided resource read, passed as a hidden argument, and assigned |
| `numeric_intrinsics` | integer and float comparison and arithmetic, numeric `to_string`, string addition, slice length |

Check each program's syntax against `Staple.md` and the existing fixtures. A program that cannot express a feature records why in a comment, rather than silently dropping it.

**Revised gate:**

- Every diagnostic family is in the ownership table. **Every family owned by 5.4 has zero stubs across the corpus**, including the standard library bodies, apart from stubs whose first blocker the table assigns to a later substage.
- Every `emits` function in the 5.4 corpus entries is fully emitted and body-identical to legacy.
- The declaration census holds, and every fully emitted body matches legacy (record the new count).
- The legacy IR comparison reports `same` after every Step 2 extraction.
- The CLI harness reports each program as blocked, identical, or compile-only, with none different.
- `rg` finds no `typed_module`, `TypedModule`, `resolved()`, `staple_syntax::Expression`, or `SyntaxId` in `codegen/lowered/`, and no call to `concrete_is_copy` in a pass-mode, provider, or target decision.
- The Contract 6 gates pass.

**Handoff:**

- Record the per-substage stub totals and the new first-blocker families; they are the starting backlog for the 5.5 and 5.6 plans.
- Record the extension points 5.5 and 5.6 fill:
  - `emit_place_pointer` for the non-symbol place kinds (5.5);
  - `emit_call_cleanup` and the moved-ownership live flag (5.6);
  - the owned-binding guard, which 5.6 removes when it emits owned-binding cleanup (5.6).
- Update the breakdown's 5.4 section (link this plan and mark its old gate as superseded) and [TYPED_LOWERING_PLAN.md](TYPED_LOWERING_PLAN.md) with one status note.

## Ordering

```text
Step 1 → Step 2 → Step 3 → Step 4 → Step 5 ─┐
                           Step 6 ──────────┼→ Step 10
                           Step 7 → Step 8 ─┤
                           Step 9 ──────────┘
```

- Step 1 comes first: without it, progress cannot be measured and the owned-binding gap stays open.
- Step 2 comes before any step that emits the operations it shares.
- Step 3 comes before Step 4, because mutation arguments need binding cells.
- Once Step 3 lands, Steps 4, 6, 7, and 9 touch different match arms and can proceed in parallel. Step 5 needs Step 4's argument assembly for hidden arguments. Step 8 needs Step 7's adapter values.
- None of the steps needs a separate plan file. Steps 4 (arguments) and 7 (closures) carry the most ABI risk and should land as several small commits, each re-running the in-process harness.
