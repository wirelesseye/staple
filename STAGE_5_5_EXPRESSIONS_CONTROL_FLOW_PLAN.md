# Stage 5.5 Plan: Expressions, Patterns, Places, and Control Flow

This is the separate plan the Stage 5.5 section of [STAGE_5_LLVM_MIGRATION_BREAKDOWN.md](STAGE_5_LLVM_MIGRATION_BREAKDOWN.md) recommends. Read the breakdown's Migration Contract (items 1–7), Decisions D1–D6, and the [Stage 5.4 plan](STAGE_5_4_CALLS_CLOSURES_RESOURCES_PLAN.md), including its post-gate review fixes, first. This plan's revised gate (Step 10) supersedes the **Gate** paragraph in the breakdown's 5.5 section.

Line references are against `734d7a3` and will drift; re-locate code by function name. Run the suite with `cargo nextest run --workspace` (the test profile is `opt-level = 1`; about two minutes).

## Starting Point

The lowered emitter covers calls, callable values, closures, resources, adapters, and the numeric/string/slice intrinsics (Stage 5.4). The 28-program differential corpus compares 5822 fully emitted bodies with legacy and reports 2240 stubs. Of those, 1903 are owned by 5.5:

| Family (first blocker) | Stubs | Notes |
| --- | --- | --- |
| `match` | 1049 | match expressions; the pattern tests behind them are hidden until this unlocks |
| `parameter destructuring` | 632 | product, nominal, and nested parameter patterns (moved here from 5.3) |
| `nominal pattern binding` | 87 | `let`/arm nominal patterns, `String`/`Ref`/distinct/sum alternatives |
| `loop value or cleanup` | 58 | loops with break values or body-result drops |
| `coercion or move` | 30 | coercions only; the move half landed in 5.4 |
| `satisfies` | 29 | |
| `assignment`, `product`, `access`, `representation place`, `string template`, `index`, `propagating pattern binding` | 18 | |

Each stub records only its body's **first** blocker. Unlocking `match` will expose pattern forms, coercions, and places inside those bodies, so measure progress by family, not by total.

**Standard-library impact.** The empty program still stubs 73 bodies: 66 blocked by 5.5 families, 6 by buffer intrinsics (5.6), and 2 by `reactive call` (5.8). 5.5 therefore unlocks most of the standard library. The runnable-empty-program milestone, which the breakdown expects after 5.6, also needs those two 5.8 reactive calls; Step 10 records this.

The legacy code this substage ports (sizes in lines):

| Legacy function | Lines |
| --- | --- |
| `compile_expression_uncoerced` | 611 |
| `compile_match_pattern_branch` | 457 |
| `compile_item` | 245 |
| `compile_place_pointer` | 217 |
| `compile_string_template` + `compile_formatter_write_literal` | 197 |
| `compile_designated_product_elements` + `compile_named_spread_product_elements` + `compile_product_elements` + `compile_product_default_plan` + product/repeated | 381 |
| `coerce_sum_value` + `store_sum_payload` + `coerce_value` + `extract_sum_alternative` + `coerce_slice_ref_value` | 288 |
| `compile_logical_expression` | 131 |
| `bind_pattern_value` + `bind_top_level_pattern` + `bind_function_parameters` | 250 |
| `compile_propagating_binding` | 118 |
| `compile_match_expression` | 116 |
| `compile_mutate_index_assignment` + `compile_assignment` | 164 |
| `compile_loop_expression` | 92 |
| `compile_index_expression` + `compile_index_load` | 105 |
| `compile_string_literal_pattern_branch` | 82 |
| `store_pattern_globals` + `predeclare_checked_bindings` + `compile_top_level_item` (pattern parts) | 230 |

## Decisions Specific to 5.5

**E1: Type-dependent layout decisions move into lowering.** Legacy decides several things at emission time from the concrete types, and those decisions are what this family consists of:

- the target sum alternative of a coercion (`coerce_sum_value` calls the checker's `select_sum_alternative`);
- the alternative a nominal or singleton pattern tests (`compile_match_pattern_branch` finds a position by comparing `Distinct` IDs and types, and asks the resolver for singleton, constructor, and `String`/`Ref` builtin identities);
- the decoded payload of a literal pattern (`string_literal::decode` on the AST text).

Contract 1 says the backend never re-derives a fact lowering could record. So Step 2 records these as lowered plans:

- a `LoweredCoercionPlan` per coercion: a recursive widening plan naming the target alternative index, or element-wise plans for products;
- a per-pattern test plan: the alternative index, whether the pattern is a `String`/`Ref`/distinct/singleton test, and the decoded literal payload.

Each plan is validated and snapshotted, and the emitter only reads it. The alternative is to share `select_sum_alternative` as a pure backend function. It is rejected because it would leave the checker's selection rule in two places, while lowering already imports it.

**E2: Cleanup positions diagnose until 5.6 (fixes the breakdown).** The breakdown's 5.5 section says loop and match cleanup "may emit no drops" until 5.6. That would silently skip cleanup, against Contract 2. Every drop position this substage reaches instead checks the owner's artifact-use record for its site, and diagnoses `<site> cleanup` (owner 5.6) when the record exists. The sites are:

- `DiscardedResult`, for an expression statement's `drop_result`;
- `ReplacedValue`, for an assignment's `drop_previous`;
- `LoopBodyResult`, for `drops_body_result`;
- `WildcardDiscard`, already handled this way in 5.4;
- the owned bindings a match arm, pattern, or loop introduces, through the existing owned-binding guard.

A body with no such record emits no drop, exactly as legacy does. The hooks keep a final signature, so 5.6 only fills in their bodies, as it does for `emit_call_cleanup`.

**E3: One place emitter, one pattern binder.** Extend 5.4's `emit_place_pointer` to cover every `LoweredPlaceKind`, and extend the existing `bind_pattern` for every pattern form. Parameter destructuring, `let` patterns, match arms, and initializer pattern bindings all go through that one binder, as legacy `bind_pattern_value` serves them all.

## Steps

Each step ends with the Contract 6 gates (using `cargo nextest run --workspace` for the suite) and a commit. A step that moves legacy code into the shared layer proves legacy IR is unchanged with `scripts/compare-llvm-ir.py` against the pre-step binary, over `staple-compiler/examples/*.sta`, `examples/game_loop/main.sta`, and the Stage 5.2 ABI/coroutine probe programs, with 16 runs for `coroutines.sta`.

### Step 1: Cleanup hooks and family hygiene

- Add the E2 hooks: `emit_drop_site(owner, ArtifactUseSite)`. It returns `Ok(())` when the owner has no use record for the site. When a record exists, it diagnoses `discarded result cleanup`, `replaced value cleanup`, or `loop body result cleanup` (all owner 5.6). Route the existing `drop_result` diagnostic through it, so a statement whose discarded value needs no drop now emits.
- Rename the family `coercion or move` to `coercion`, since the move half landed in 5.4, and update the ownership table.
- The breakdown's 5.5 section already states E2 in place of the old "may emit no drops" sentence (changed when this plan was written).

### Step 2: Lowering-side plans (E1)

- **`LoweredCoercionPlan`**, stored beside `LoweredExpression::coercion`. It records `Identity`, `SumWiden { alternative, payload: Box<plan> }`, `ProductElements(Vec<plan>)`, `SliceRef`, `Distinct` (representation), and whatever else `coerce_value`/`coerce_sum_value`/`coerce_slice_ref_value` distinguish. Read those three functions first to make the variant list exhaustive. Lowering computes the plan with the same `select_sum_alternative` rule. Instance bodies store it with the concrete types, recomputed at materialization like the Stage 3.4 derived facts.
- **Pattern test plans** on `LoweredPattern`. For each pattern form `compile_match_pattern_branch` handles, record:
  - the sum alternative index it tests, if any;
  - the builtin identity (`String`, `Ref`, distinct representation, singleton);
  - the decoded literal payload: string bytes, an integer or float value with its type, or a boolean.
- **Validation and tests.** Validators check that each plan agrees with the value types it connects. The normalized snapshot renders them. A transition test compares each plan with the legacy decision: `select_sum_alternative` for coercions, and the position legacy computes for patterns, over the corpus and the existing `lower.rs` pattern and coercion fixtures.

### Step 3: Share the family's legacy helpers (Contract 7)

Move the `TypedModule`-free cores into `codegen/ir.rs` and switch legacy to them, so that legacy reads the Step 2 plans' inputs through the same builders:

- sum storage: `store_sum_payload`, `extract_sum_alternative`, the tag load and compare, and the payload reinterpretation;
- the slice-ref coercion builder;
- the string-literal pattern comparison (length check, then `memcmp`);
- `compile_formatter_write_literal`;
- the logical short-circuit phi builder, if it has a type-independent core.

The legacy IR comparison must report `same`.

### Step 4: Coercions, `satisfies`, products, and access

- Apply `LoweredCoercionPlan` wherever `LoweredExpression::coercion` is `Some`, including call arguments and returns. This replaces the `coercion` diagnostic.
- `Satisfies` is transparent: emit its operand, and let the parent header's coercion apply.
- **Products:** replay `LoweredProduct::steps` in order (`Positional`, `Designated`, `PositionalSpread`, `NamedSpread`, `Default`), then assemble `fields` in final layout with the shared `build_product_value`. Handle the one-element collapse as legacy does. `RepeatedProduct` follows `count`/`collapsed`.
- **Access:** `Representation`, `Product`, `Slice`, and `Scalar`, each after its `dereference` chain of `Ref` loads.

### Step 5: Places, assignment, and index reads

- **`emit_place_pointer`:**
  - `Temporary`: an alloca holding the evaluated expression;
  - `Dereference`: follow the chain;
  - `ProductElement`: a GEP, with `slice` for slice elements;
  - `Representation`: the distinct payload;
  - `Indexed`: dispatch through `MutateIndex` or `Index` by the owner's binding for that site.
- **Assignment items:** evaluate the value, get the place pointer, then:
  - run `emit_drop_site(ReplacedValue)` when `drop_previous` is set;
  - store;
  - update `initialization_symbol` state;
  - diagnose `signal_notify` as owned by 5.8.

  `mutate_index` dispatches through the `IndexedAssignment` binding (`compile_mutate_index_assignment`).
- **Index reads:** call the `Index` binding's target (an instance, or a structural artifact whose body is 5.7), honoring `whole_temporary`/`base_temporary`/`index_temporary`.
- Add the focus follow-ups `bump` (`calls_mutation`) and `bump` (`resources_with`) to their `emits` lists.

### Step 6: Patterns and parameter destructuring

- **`bind_pattern` covers every form:**
  - product patterns, including one-element collapse, `mutable`, and `moved`;
  - nominal patterns: `String`, `Ref` (payload load), distinct representation, and sum alternatives, all through the Step 2 test plan;
  - literal patterns, in irrefutable positions only;
  - `at` patterns;
  - bindings that are `mutable` or `moved`, into 5.4's binding cells, with owned droppable bindings still stopped by the guard.
- **Parameter destructuring:** replace the 5.4 guard with the real traversal. `bind_function_parameters` binds the parameter pattern over the flattened parameters and pointers, as legacy does.
- **Initializer pattern bindings:** module globals through `store_pattern_globals` and the pattern initialization state (`store_pattern_initialization_state`).
- Add `sum3` and `pair_of` to `calls_curried_defaults`' `emits` list.

### Step 7: Match, logicals, and propagation

- **Match:** evaluate the subject once. Emit each arm's conditional test from its pattern test plan (Step 2): tag compares, nested product tests, string-literal comparisons through the shared builder, and literal compares. Bind the arm through `bind_pattern`, evaluate the body, and merge the arm values with a phi of the match's type. A non-matching fall-through ends in legacy's trap or `unreachable`. Mirror `compile_match_expression`/`compile_match_pattern_branch` block structure and names, so the body comparison holds.
- **Logicals:** short-circuit on the `Bool` alternative at `true_index`.
- **Propagation:** a `PatternBinding` item with `propagating` tests the success alternative (`CheckedPropagation::success_index`). On failure it returns the failure value, converted to the function's result type through its coercion plan, as `compile_propagating_binding` does.

### Step 8: Loops with values

`LoweredLoop` with a non-`Never` result, `break` items with values (phi incoming per legacy's loop context), `continue`, `body_falls_through`, and `drops_body_result` through `emit_drop_site(LoopBodyResult)`. Replace 5.3's restricted never-returning loop, keeping its block names.

### Step 9: String templates

Call the `FormattingConstructor`, `FormattingWrite`, and `FormattingFinish` bindings. Write literal parts through the shared `compile_formatter_write_literal` core. Call each interpolation's `Interpolation` binding, which is an instance or a structural `Debug` artifact (whose body stays a 5.7 stub). Follow `compile_string_template`'s order and names exactly.

### Step 10: Corpus, gate, and handoff

**Corpus additions** (tagged `5.5`, `MayBeBlocked`, each with an `emits` list naming its own functions):

| Program | Covers |
| --- | --- |
| `match_sums_products` | nested matches over sums of products, singleton and nominal alternatives, `at` patterns, a wildcard arm |
| `match_strings_literals` | string-literal patterns and integer literal patterns |
| `destructuring` | parameter and `let` destructuring of nested products and nominal types |
| `places_assignment` | assignment through product elements, representations, dereferences, and indexed places (`MutateIndex`) |
| `coercions` | sum widening in returns and arguments, and slice-ref coercion |
| `loops_values` | `break` with values, `continue`, and nested loops |
| `propagation` | a propagating binding returning the failure alternative |
| `templates` | string templates with display and debug interpolations |

Check each program's syntax against `Staple.md` and the existing fixtures.

**Revised gate:**

- Every diagnostic family is in the ownership table, and **every 5.5-owned family has zero stubs across the corpus**.
- Every `emits` function in the 5.4 and 5.5 corpus entries, including the four follow-ups, is fully emitted and body-identical to legacy.
- The declaration census holds, and every fully emitted body matches legacy (record the count).
- The Step 2 transition test agrees with the legacy decisions.
- The legacy IR comparison is `same` after Step 3.
- The CLI harness reports each program as blocked, identical, or compile-only, with none different.
- `rg` finds no `typed_module`, `TypedModule`, `resolved()`, `staple_syntax::Expression`, `SyntaxId`, or `select_sum_alternative` in `codegen/lowered/`.
- The Contract 6 gates pass.

**Handoff:**

- Record per-substage stub totals and the empty program's remaining blockers. Expected: buffers (5.6), GC finalizer bodies (5.6), and the two `reactive call` bodies (5.8).
- Update the breakdown's 5.6 section. Its runnable-empty-program requirement also needs those two 5.8 reactive calls, so either 5.6 ports them early (they are plain runtime calls) or the requirement moves to 5.8. Record which, with the reason.
- Record the E2 hooks 5.6 fills in: `emit_drop_site`'s bodies, `emit_call_cleanup`, and the owned-binding guard.
- Update [TYPED_LOWERING_PLAN.md](TYPED_LOWERING_PLAN.md) with one status note.

## Ordering

```text
Step 1 → Step 2 → Step 3 → Step 4 → Step 5 → Step 6 → Step 7 → Step 10
                                     │                 ↑
                                     ├→ Step 8 ────────┤
                                     └→ Step 9 ────────┘
```

- Steps 1–3 are prerequisites. Step 2 changes lowering, so it runs the full lowering validator and transition tests before anything reads it.
- Step 4 comes before everything that produces or consumes values of widened types. Step 5 comes before Step 6, because patterns with `mutable` bindings and assignment share places. Step 6 comes before Step 7, because match arms bind through `bind_pattern`.
- Steps 8 and 9 depend only on Step 4 and can run in parallel with Steps 5–7.
- None of the steps needs a separate plan file. Steps 6 and 7 carry the most risk, since they port the 457-line `compile_match_pattern_branch`. Land them as several small commits (one pattern form at a time), re-running the in-process harness each time. The body comparison catches block-structure drift immediately.
