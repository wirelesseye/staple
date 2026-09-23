# Stage 2.5 Plan: Lower Calls, Trait Evidence, and Closures

## Goal and starting point

Replace Stage 2.4's `Deferred(Callable)` expressions with owned call and callable-value nodes. Each call must state what is called, how arguments are evaluated and passed, which checked function type applies, which resources are required, and what trait evidence remains to be realized after generic substitution.

Stage 2.2 already catalogs functions, symbols, trait methods, implementations, and ordered captures. Stage 2.3 records places and mutation facts. Stage 2.4 gives expressions occurrence-aware keys, complete ordinary child nodes, checked `Index` dispatch recipes, and formatting selections. Build on these records. Keep the private legacy backend bridge while this stage is implemented.

## Completion gate

- No `Deferred(Callable)` expression remains, including callable-valued names and selectors, function expressions, and calls inside contextual defaults.
- Every call has exactly one explicit target category and an owned, ordered argument plan. Every callable value has an explicit construction plan.
- Every trait call, index read, indexed mutation, and formatting interpolation has a complete evidence recipe or a source-based diagnostic.
- Generic templates can retain declared type/effect parameters; call-site substitutions and residual obligations are recorded structurally. No `FunctionInstanceId` is assigned here.
- Validation checks targets, evidence, captures, argument shape, resource order, and arena references. Repeated lowering is deterministic, and the workspace suite remains green.

## Scope and boundaries

The callable categories are direct known function, indirect closure, external function, intrinsic, constructor, explicit trait implementation, structural trait method, and compiler helper. A direct known function can still need a closure environment for same-function recursion; the category alone does not imply a null environment.

This stage lowers `Expression::Call` and `Expression::Function`, plus `Name` and `Access` occurrences deferred as callable values. It completes the callable side of Stage 2.4's `LoweredIndex`, the `MutateIndex` dispatch on Stage 2.3 assignments/places, and the trait methods selected for string interpolation. It does not lower `Resource`, `With`, `Coro`, or `Await`; Stage 2.6 owns those nodes and their lexical resource binding and coroutine plans. Stage 2.5 records ordered resource requirements at each call so Stage 2.6 can bind them to lexical providers.

Stage 3 interns concrete instances and resolves evidence after substitution. Stage 4 catalogs generated adapters and helpers. Stage 5 changes LLVM emission. These later responsibilities must not be simulated with backend symbol strings or debug-formatted specialization keys in Stage 2.5.

## Proposed IR

- `LoweredCallableTarget`: tagged variants for the eight categories above. Variants carry semantic `FunctionId`, `SymbolId`, `TypeId`, `TraitId`, or `TraitMethodId` as appropriate, not an LLVM symbol. An indirect call carries its callee `ExpressionId`.
- `LoweredCall`: origin; target; callee evaluation or direct-target reference; checked post-inference `CheckedFunctionType`; ordered visible arguments; ordered hidden `CheckedResource` requirements; checked mutations/moves; initialization checks; temporary and writeback/drop facts; result type; and the site-specific substitution/evidence recipe.
- `LoweredCallStep`: an ordered step for callee evaluation, argument evaluation, product field/spread/default placement, hidden resource lookup, and invocation. Explicit source arguments execute in source order; defaults execute in the order selected by the checked plan. Store final ABI slot mapping separately from evaluation order.
- `LoweredCallableValue`: function/constructor/extern/intrinsic or trait target, checked function type, adapter kind, and optional `LoweredClosureConstruction`.
- `LoweredClosureConstruction`: target function, ordered captures from the function catalog, per-capture type and by-value/borrowed/shared-cell access, environment reuse for recursion, ownership/drop responsibility, and adapter requirement. Record semantic inputs only; target-specific environment layout stays in LLVM.
- `TraitEvidence`: selected explicit implementation and method function; selected structural method with completed trait arguments; or a declared bound/prerequisite with its type/effect parameter substitutions. Keep negative implementations as rejection data, not callable targets.
- `CallSubstitutions`: structural mapping from relevant `TypeParameterId`s to checked type/effect arguments, including outer parameters captured by nested functions. Retain unresolved declared template parameters rather than inventing a concrete instance.

The precise Rust shapes may vary, but their relationships must be typed arena/catalog references. Do not retain cloned `Expression` nodes or source `Type` syntax in the new payloads.

## Implementation sequence

### Step 1 — Inventory checked call decisions and add the IR skeleton

- Enumerate every runtime call route in `codegen.rs`: juxtaposed chain, curried defaults, trait dispatch, intrinsic, generic direct call, declared/global/extern call, indirect closure, and constructor or primitive macro call.
- Map each route to checked inputs (`juxtaposed_call_plan`, `curried_default_plan`, `trait_dispatch_for`, `symbol_for`, function/symbol catalogs, expression type/effects, and product default plans). Identify any decision currently made only by the backend and add a narrow checked record before removing that decision from lowering's inputs.
- Add typed callable, call-step, closure-construction, substitution, and evidence records. Extend arena validation and ownership traversal to recognize their child IDs.
- Add a decision table test that covers each route and fails when the explicit category is missing.

Gate: the schema represents every existing route without a generic “unknown callable” fallback.

After this step, update `TYPED_LOWERING_PLAN.md` with general progress and `STAGE_2_LOWERING_BREAKDOWN.md` with detailed progress.

### Step 2 — Lower callable values and closure construction

- Replace callable-valued `Name`/`Access` deferrals and `Function` deferrals using resolved symbols, checked function types, and the Stage 2.2 function catalog.
- Distinguish declared functions, externs, intrinsics, constructors, trait methods, and local function bindings. Retain initialization checks and contextual generic function types.
- Copy captures in catalog order, with their borrowed, non-owning, shared-cell, and move/drop facts. Record whether construction uses a fresh environment, no environment, or the current closure environment for recursion.
- Record the adapter kind needed for constructor values, extern function values, and curried or nested closures. Stage 4 will catalog concrete generated adapters.
- Validate capture symbols and ownership against the function and symbol catalogs.

Gate: named, anonymous, nested, generic, borrowed, mutable-cell, recursive, constructor, extern, and intrinsic function values each have an explicit construction plan.

After this step, update both plan files.

### Step 3 — Lower ordinary, direct, indirect, external, and intrinsic calls

- Lower callee and arguments in the order the current backend evaluates them. Use the selected symbol and checked callee type to choose the target category; local closure bindings remain indirect.
- Record the checked call-site function type and the signature used for argument adaptation, including curried layers and juxtaposed parameter style.
- Turn mutation and move markers into per-slot pass modes: value, borrowed pointer, mutable place, or materialized temporary. Reuse Stage 2.3 place IDs where available; record temporary cleanup and writeback.
- Record initialization checks and the ordered hidden effect resources from the checked function type, including mutability and ownership mode. Provider resolution is completed in Stage 2.6.
- Preserve extern ABI and C-string temporary lifetime rules as explicit call facts. Record intrinsic identity and argument mapping, without asking LLVM to recognize a source call shape.

Gate: direct/indirect, generic, external, intrinsic, recursive, move-only, mutable-place, and resource-bearing calls can be reconstructed from lowered data.

After this step, update both plan files.

### Step 4 — Normalize juxtaposed chains, curried defaults, and call arguments

- Consume `CheckedJuxtaposedCallPlan` once at the outer call, using its `consumed_calls`, checked function type, and ordered argument list. Mark inner chain nodes as consumed by that owner so they are not emitted again.
- Lower curried default calls as ordered applications with each default's checked function type, then the explicit argument or the `_` partial-application result.
- Reuse Stage 2.4 product evaluation and final-slot plans for designated arguments, positional/named spreads, and contextual defaults. Give a default applied at a call its own occurrence key and expected slot type.
- Keep callee evaluation, visible argument evaluation, default evaluation, resource lookup, and invocation order explicit. Assert each final parameter slot is filled exactly as the checked plan requires.

Gate: multi-layer curried and juxtaposed calls, companion receiver syntax, partial application, defaults, spreads, and argument side effects preserve the checked evaluation order.

After this step, update both plan files.

### Step 5 — Lower constructor and compiler-provided call routes

- Resolve constructor calls and constructor values through selected symbol and type IDs; record recursive managed-reference construction separately from ordinary nominal wrapping.
- Record singleton values as values rather than constructor calls.
- Normalize the compiler-provided `c_string` primitive call to its owned C-string value or a typed intrinsic operation, consistent with Stage 2.4's decoded `CString` payload and interior-NUL diagnostics.
- Record compiler helper call identities used by currently checked operations. Leave complete helper discovery and deduplication to Stage 4.

Gate: constructors, first-class constructor adapters, recursive `Ref` construction, singletons, and the C-string primitive have explicit typed targets without backend AST matching.

After this step, update both plan files.

### Step 6 — Build trait evidence recipes and attach them to all sites

- For each selected `CheckedTraitDispatch`, record owning trait/method IDs, completed functional-dependency arguments, instantiated method type, and relevant substitutions.
- Select explicit implementation and selected method when checked arguments determine one. For generic arguments whose choice depends on later substitution, record the declared bound or implementation prerequisite that Stage 3 must realize; never choose an implementation by a display name.
- Record structural evidence with the `StructuralTraitMethod` and structural argument types. Include default methods and prerequisite chains, keeping their selected function IDs where known.
- Apply the same evidence form to explicit trait calls, Stage 2.4 index reads, Stage 2.3 `MutateIndex` assignments/places, and string-template display/debug interpolations.
- Diagnose missing, contradictory, or incomplete evidence at the source origin, with generic bound recipes retained where selection is intentionally deferred.

Gate: every trait-dependent lowered operation has one validated evidence recipe, and Stage 3 can realize it after substitution without resolver or type-checker lookup.

After this step, update both plan files.

### Step 7 — Validate coverage, determinism, and transition behavior

- Reject remaining `Deferred(Callable)` nodes and any call with an unresolved target category, absent checked function type, incomplete argument layout, missing resource requirement, invalid substitution, or dangling semantic/arena reference.
- Check each direct target against the function/symbol catalogs; check implementation method IDs, structural methods, prerequisites, and closure captures against their catalogs and declared bounds.
- Validate that each call owns the right callee/argument occurrences, including consumed juxtaposed calls and contextual defaults. Extend the existing ownership traversal and normalized repeated-lowering snapshot to callable nodes and evidence.
- Add transition comparisons against checked call plans, selected symbols, trait dispatches, function signatures, capture order/ownership, and resource order.
- Run the full workspace suite and representative CLI compile/run, object emission, and LLVM verification coverage already exercised by it.

Gate: all accepted fixtures lower with complete callable information, all focused validation tests pass, and Stage 2.6 is the only remaining source of deferred runtime expressions.

After this step, mark Stage 2.5 complete in both plan files and identify Stage 2.6 as next.

## Testing matrix

- Calls: known direct, local indirect, extern, intrinsic, generic, recursion, zero-argument, curried, juxtaposed, companion, partial, defaults, spreads, and initialized globals.
- Arguments: source order versus ABI slot order, mutation/move markers, borrowed and owned parameters, addressable places, temporary materialization, writeback, early exits, and C-string lifetime.
- Callable values: named and anonymous functions, constructors, extern adapters, nested generic closures, borrowed captures, captured cells, resource captures, and recursive environment reuse.
- Trait evidence: explicit and generic implementations, default methods, prerequisites, functional dependencies, structural indexing/debug/iteration, negative implementations, and missing evidence diagnostics.
- Transition sites: index reads, `MutateIndex` writes, formatting interpolations, implicit thunks, and generated helper calls.
- Determinism: repeated lowering of overloaded/generic programs yields identical callable/evidence snapshots and source-ordered dependencies.

After each completed step:

```text
cargo fmt --all -- --check
cargo check --workspace
cargo test -p staple-compiler lower::tests
git diff --check
```

At the Stage 2.5 completion gate, also run `cargo test --workspace`.

## Known risks and decisions

- Current direct-versus-indirect behavior can depend on local binding state and recursive closure environment reuse. The lowered target and environment plan must encode those facts explicitly.
- `CheckedJuxtaposedCallPlan` contains source expressions and consumes nested call nodes. Lowering must replace them with occurrence-aware IDs and one owner, without double evaluation.
- `CheckedCurriedDefaultPlan` also contains source expressions. Call-site defaults need distinct occurrences and checked expected types, as product defaults do.
- Trait selection after generic substitution can differ from a premature concrete choice. Evidence recipes must preserve the declared obligation until Stage 3 has canonical substitutions.
- `compile_effect_arguments` currently infers indirect passing from copyability, mutations, moves, and whether an argument is a place. Stage 2.5 must record the resulting pass mode and temporary cleanup on each argument.
- The checked effect row gives ordered resource requirements; Stage 2.6 resolves lexical providers. Keep requirement order stable now.
- Some helper dependencies are currently discovered inside LLVM emission. Stage 2.5 records semantic references available at call sites; Stage 4 completes the generated artifact catalog.

## Deliverables

- Explicit call targets, ordered argument/pass plans, and call-site substitutions.
- Callable-value and closure-construction nodes with capture ownership and adapter requirements.
- Trait evidence recipes attached to trait calls, indexing, mutation, and formatting sites.
- Expanded source-based validation, coverage, transition comparisons, and deterministic snapshots.
- General progress in `TYPED_LOWERING_PLAN.md` and detailed progress in `STAGE_2_LOWERING_BREAKDOWN.md` after each implementation step.
