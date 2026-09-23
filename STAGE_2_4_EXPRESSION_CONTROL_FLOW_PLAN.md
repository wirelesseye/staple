# Stage 2.4 Plan: Lower Ordinary Expressions and Control Flow

## Goal

Replace every Stage 2.3 `LoweredExpressionKind::Unlowered` node in the ordinary-expression families with an owned, typed lowered form. The result must preserve source evaluation order, checked semantic selections, control-flow exits, and cleanup-relevant ownership facts without cloning source expressions or asking later phases to reconstruct type-checker decisions.

Stage 2.4 covers literals, ordinary value names, structural access, products/defaults/spreads, repeated products, blocks, `satisfies`, logical operators, loops, matches, indexing, strings, C strings, and string templates.

Stage 2.4 does not resolve function values, closures, calls, or callable evidence (Stage 2.5), and it does not lower `with`, resource access, `coro`, or `await` operations (Stage 2.6). Those nodes must be marked with an explicit deferred family instead of remaining indistinguishable `Unlowered` nodes.

## Completion Criteria

- Every reachable ordinary source expression has one concrete lowered representation with typed child arena IDs.
- Every expression encountered during traversal is classified as lowered in Stage 2.4, deferred to Stage 2.5/2.6, or rejected as a compile-time-only survivor.
- Product evaluation plans encode source evaluation order separately from final field layout, including designated fields, positional spreads, named spreads, and defaults.
- Logical, loop, and match nodes contain their checked branch/match facts and lowered blocks, arms, and patterns.
- Structural access and indexing contain their selected checked plans; no later phase needs resolver lookup to distinguish representation, product, slice, scalar, or trait-dispatched access.
- Literal payloads are decoded/validated during lowering and are not reparsed by LLVM generation.
- Validation detects dangling children, incomplete product plans, invalid control-flow ownership, missing checked plans, and unexpected deferred families.
- Repeated lowering produces identical normalized arena snapshots.
- Source behavior and ABI remain unchanged; the LLVM backend continues using the private legacy payload.

## Scope and Family Ownership

Stage 2.4 owns these syntax variants:

- `Satisfies`, `Match`, `Loop`, `Block`, `Product`, `RepeatedProduct`, `Access`, `Index`, `Logical`, `Name`, `String`, `StringTemplate`, `CString`, `Integer`, and `Float`.
- `Name` or `Access` nodes that denote ordinary values or singletons are completed here.
- `Name` or `Access` nodes that denote functions, constructors, companion methods, or other callable values are explicitly deferred to Stage 2.5.

Later stages own:

- Stage 2.5: `Function`, `Call`, and callable-valued `Name`/`Access` nodes.
- Stage 2.6: `Resource`, `With`, `Coro`, and `Await`.

`Unary`, `Binary`, `SyntaxArgument`, `VisibilityArgument`, `Quote`, and `Splice` remain illegal after their earlier compiler phases and must produce lowering diagnostics.

The Stage 2 breakdown's phrase “every non-call, non-closure expression” is interpreted with these explicit Stage 2.5/2.6 exclusions; the final coverage test must make that partition exhaustive.

## Representation Additions

Extend the private lowered schema with the following concepts. Exact Rust names may change to fit the implementation, but the information and invariants are required.

### Expression identity and deferral

- Replace the syntax-only expression memo with an occurrence-aware key.
- Primary AST occurrences use their source syntax and lowered owner.
- Contextual/default expressions additionally include the consuming product expression and destination slot, so one declaration default may be lowered for multiple checked contexts without aliasing incompatible node types.
- Replace `Unlowered` with `DeferredExpressionFamily::{Callable, Resource, Coroutine}` once Stage 2.4 traversal begins. At the Stage 2.4 gate, no generic `Unlowered` node may remain.

### Ordinary values and literals

- `LoweredName`: symbol ID, initialization-check requirement, access/storage mode, move/borrow facts, and singleton identity when applicable.
- `LoweredInteger`: parsed magnitude and selected `IntegerType`.
- `LoweredFloat`: selected `FloatType` plus a stable payload representation that preserves the checked finite value.
- `LoweredString`: decoded UTF-8 bytes.
- `LoweredCString`: decoded bytes with the required trailing NUL represented explicitly and no interior NUL.

### Access and coercion

- `LoweredAccess::{Representation, Product, Slice, Scalar}` with base expression, selected index where applicable, and the ordered dereference payload types copied from `CheckedAccess`.
- `LoweredSatisfies` with its child expression. The parent expression header remains authoritative for the checked coercion.
- Coercion remains explicit on every expression header; validation checks that its source/target agree with the node and child where the relation is statically available.

### Products

- `LoweredProduct` with final checked product shape, ordered evaluation steps, and ordered final fields.
- Each evaluation step identifies a child expression and one of:
  - direct positional placement;
  - designated placement;
  - positional spread with explicit source-index to destination-slot mappings;
  - named spread with explicit source-field to destination-slot mappings;
  - default evaluation for a specific destination slot and contextual expected type.
- Keep source evaluation order separate from final layout order. Explicit source elements execute left-to-right; missing defaults execute afterward in final field order, matching the current backend.
- `LoweredRepeatedProduct` stores one child expression, the checked repeat count derived from the result type, and whether the single-element representation collapses to the child.

### Control flow

- `LoweredLogical`: operator, left/right expressions, checked Bool type, and resolved true alternative index.
- `LoweredLoop`: lowered body block, result type, whether the body result needs dropping before the back edge, and cleanup-boundary metadata required by break/continue.
- `LoweredMatch`: subject expression, checked source type, ordered arms, and result type.
- `LoweredMatchArm`: origin, lowered pattern, lowered body expression, bound symbols, and arm cleanup boundary.
- Existing lowered `Break`/`Continue` items remain the exits used inside loop bodies; validation checks they are owned by an enclosing lowered loop.

### Indexing and templates

- `LoweredIndex`: base and index expressions in evaluation order plus the complete checked trait dispatch currently stored in `CheckedTraitDispatch`.
- Stage 2.4 copies the checked dispatch and instantiated argument types; Stage 2.5 converts it into the final callable/evidence category.
- `LoweredStringTemplate`: ordered literal and interpolation parts.
- Each interpolation stores the child expression, display/debug format, selected trait/method IDs, and checked value type. Stage 2.5/4 may attach callable evidence/helper catalog entries without looking names up again.
- Record formatter constructor/write/finish semantic function IDs through checked metadata rather than `standard_function_named` lookup during later emission. If the checker does not currently retain these selections, add narrow checked side-table entries.

## Implementation Sequence

### Step 1 - Establish Exhaustive Expression Dispatch and Occurrence Identity (Done)

- Add a single lowering dispatcher covering every `Expression` variant.
- Introduce explicit Stage 2.5 and Stage 2.6 deferred kinds.
- Refactor expression memoization so ordinary source occurrences still deduplicate, while contextual/default occurrences have distinct stable keys.
- Define expression ownership for module initializers, function templates, nested blocks, match arms, and contextual defaults.
- Make recursive lowering replace a node kind only after all required children succeed, avoiding partially initialized arena nodes after diagnostics.
- Add a coverage classifier test that enumerates every syntax variant and fails when a new variant has no owned/deferred/rejected decision.

Gate: all Stage 2.3 fixtures traverse recursively with no ambiguous `Unlowered` nodes, and duplicate ordinary references still reuse their arena ID.

Completed:

- `classify_expression` is an exhaustive match returning `Ordinary(Stage24Family)`, `Deferred({Callable, Resource, Coroutine})`, or `Rejected`; `Unlowered` was removed and not-yet-lowered families carry explicit `Pending(family)` markers instead.
- `ExpressionKey { syntax, owner: ExpressionOwner, context: ExpressionContext }` replaces the syntax-only memo. `ContextualDefault { consumer, slot }` gives shared default AST nodes distinct keys; block lookups use the same key so a block reached again reuses its arena node.
- `lower_expression` lowers children before allocating the parent and rejects compile-time-only survivors with source diagnostics.
- Recursive traversal covers blocks, satisfies values, match subjects and arm bodies, loop bodies, product/spread elements, repeated-product elements, access bases, index operands, logical operands, and template interpolations. Loop/match payload structures land in Step 5.
- `coverage_classifier_decides_every_expression_variant` constructs one representative per variant and checks the decision table; `expression_variant_name` keeps the enumeration compile-time exhaustive.
- New tests `dispatcher_defers_later_stage_families_explicitly` and `occurrence_keys_deduplicate_ordinary_expressions_and_blocks` cover deferral classification and occurrence reuse.
- Verified with `cargo fmt --all -- --check`, `cargo check --workspace`, `cargo test -p staple-compiler lower::tests` (48 tests), `cargo test --workspace`, and `git diff --check`.

### Step 2 - Lower Scalars, Ordinary Names, and Structural Access (Done)

- Parse and validate integer and float literals once, retaining their selected checked scalar type.
- Decode strings and C strings once; diagnose invalid encodings/ranges at their source origins.
- Lower ordinary names using the selected `SymbolId`, catalog storage facts, initialization checking, move metadata, and singleton identity.
- Detect callable-valued names from the Stage 2.2 symbol catalog and mark them deferred to Stage 2.5 rather than lowering them as ordinary loads.
- Lower representation/product/slice/scalar access from `CheckedAccess`, recursively lowering the base first and copying dereference paths and indices.
- For symbol-selected access, distinguish ordinary selected values/singletons from companion or function values deferred to Stage 2.5.
- Add transition comparisons against `symbol_for`, `requires_initialization_check`, `access_for`, and literal decoding.

Gate: literals, locals/globals, mutable/captured values, singletons, representation access, product fields, scalar shortcuts, and fixed slice positions lower without resolver or type-side-table lookup afterward.

Completed:

- `LoweredExpressionKind` gained concrete `Name`, `Integer`, `Float`, `String`, `CString`, and `Access` payloads; the corresponding Stage 2.4 family arms no longer produce `Pending` markers.
- `LoweredInteger`/`LoweredFloat` store the parsed magnitude/value with the checked `IntegerType`/`FloatType`, validating range and finiteness at the source origin. `LoweredString` stores decoded UTF-8 and `LoweredCString` stores decoded bytes with the trailing NUL and no interior NUL.
- `LoweredName` records the catalog storage class, per-occurrence initialization checking, mutable/captured-cell access, movement, move-parameter status, and singleton identity. Function, constructor, and trait-dispatch callable values (including `Trait.method` selectors and companion methods) become explicit `Deferred(Callable)` nodes.
- `LoweredAccess` copies `CheckedAccess` into `Representation`, `Product`, `Slice`, and `Scalar` forms with the base expression, selected index, and ordered dereference payloads.
- Discovery: non-generic `const` bindings are ordinary runtime globals (the backend materializes and reads a module global for every reference), so they now enter the symbol catalog as `GlobalStorage` instead of being treated as compile-time-only. `snapshot` populates the symbol catalog before module initializers and function bodies so name lowering reads catalog facts.
- Two integration tests moved from codegen-time to lowering-time rejection: interior-NUL C strings now diagnose during lowering.
- New focused tests: scalar payload decoding plus checked-type transition comparisons, ordinary name storage/singleton/captured-cell facts with callable deferral, all four access forms with `CheckedAccess` comparisons, and invalid literal diagnostics.
- Verified with `cargo fmt --all -- --check`, `cargo check --workspace`, `cargo test -p staple-compiler lower::tests` (52 tests), `cargo test --workspace` (1000 tests), and `git diff --check`.

### Step 3 - Lower Products, Spreads, Defaults, and Repeated Products (Done)

- Lower explicit product elements left-to-right and build a separate final-layout plan.
- Resolve designated fields to final checked slots during lowering.
- Expand positional spreads into explicit source-index/destination-slot mappings using the operand's checked product type.
- Expand named spreads into explicit name/source-index/destination-slot mappings and preserve override behavior in source order.
- Copy `CheckedProductDefaultPlan` into owned lowered form. Lower missing defaults in final slot order using occurrence-aware keys and the plan's contextual element type.
- Ensure explicit values override earlier spread values exactly as today, while defaults only fill absent slots.
- Derive repeated-product count from the checked result type, evaluate the element exactly once, and record representation collapse for count one.
- Reject variadic, missing, duplicate, or out-of-range layouts as lowering diagnostics rather than backend failures.

Gate: anonymous/named products, contextual designated fields, positional and named spreads, defaults, overrides, empty/singleton products, and repeated products can be reconstructed and evaluated solely from lowered plans.

Completed:

- `LoweredProduct` stores the final checked shape, ordered `LoweredProductStep` evaluations (`Positional`, `Designated`, `PositionalSpread`, `NamedSpread`, `Default`), and one expression per final slot. Replaying the steps reproduces the final layout, so later overrides win while every source element keeps its evaluation position.
- Positional and designated products expand spreads through the operand's checked product type into explicit source-index/destination-slot mappings; named spreads expand into name/source-index/destination-slot mappings preserving source-order overrides. `...=` products reject missing, unknown, or non-fixed operand fields as lowering diagnostics.
- Contextual defaults copy `CheckedProductDefaultPlan`, lower in final slot order under `ContextualDefault { consumer, slot }` occurrence keys with the plan's slot type as the root override, and only fill slots no explicit or spread value initialized.
- `LoweredRepeatedProduct` records the checked count and the `count == 1` representation collapse; the element is evaluated exactly once.
- Unreachable (diverged) products still lower every element with a positional fallback shape so no child is dropped.
- New focused tests cover positional spread mappings plus contextual defaults and occurrence keys, designated slot resolution with later-wins overrides, named-spread field remapping and overrides, and repeated-product count/collapse.
- Verified with `cargo fmt --all -- --check`, `cargo check --workspace`, `cargo test -p staple-compiler lower::tests` (56 tests), `cargo test --workspace` (1000 tests), and `git diff --check`.

### Step 4 - Finish Blocks, `satisfies`, and Coercion Boundaries (Done)

- Recursively finalize every expression referenced by the `LoweredBlock` structures created in Stage 2.3.
- Preserve block item order, tail-result identity, divergence, and cleanup boundaries.
- Lower `satisfies` as an explicit wrapper referencing its value while retaining the checked coercion on the parent header.
- Validate that block tails and expression-statement discard/drop facts remain consistent with Stage 2.3 metadata.
- Add focused cases for nested blocks, early return, propagation-generated exits, and coercions involving sums, string literal widening, refs/slices, and nominal representations.

Gate: no ordinary child reachable through a block or `satisfies` node remains generic `Unlowered`, and cleanup-relevant values retain the same ownership behavior as the legacy path.

Completed:

- `LoweredSatisfies` wraps the lowered value transparently; the parent header keeps the checked coercion.
- Block results, expression statements, and all ordinary children are already finalized through the recursive dispatcher; block memoization keeps nested and function-body blocks shared.
- Validation additions: a block result may never also appear as an expression-statement item; every expression statement's discard/drop fact must agree with its checked value type; `satisfies` coercion targets must agree with the parent checked type; `satisfies` and block coercion sources must agree with the coerced child type. (Block target comparison is skipped because a block may be re-checked while inference converges and can retain an earlier coercion.)
- New focused tests cover the `satisfies` wrapper, integer widening, sum-injection and `Ref`-to-`Slice` coercions on deferred headers, nested block results, early return divergence, propagation-generated exits, and drop facts for discarded owned values.
- Verified with `cargo fmt --all -- --check`, `cargo check --workspace`, `cargo test -p staple-compiler lower::tests` (58 tests), `cargo test --workspace` (1000 tests), and `git diff --check`.

### Step 5 - Lower Logical Operators, Loops, and Matches

- Lower logical operands in left-to-right order and copy `CheckedLogical`.
- Resolve and store the true-alternative index during lowering; reject malformed checked Bool metadata with a source diagnostic.
- Lower loop bodies using the existing block/item arenas. Record body-result drop requirements and the cleanup boundaries used by break/continue.
- Track loop ownership while lowering so break/continue items can be validated against their enclosing loop.
- Lower match subjects first, then arms in source order.
- Reuse Stage 2.3 pattern lowering for every match arm and copy `CheckedMatch::source`.
- Record arm-bound symbols and cleanup boundaries needed to restore branch-local ownership state.
- Preserve divergent arms and loops without manufacturing a runtime value; use the existing `Never` convention for unreachable expressions.

Gate: short-circuiting, loop result joins, nested loop exits, exhaustive sum/product/literal matches, at-patterns, nominal destructuring, divergent arms, and cleanup-sensitive branches are fully represented without source AST traversal.

After this step, update both plan files.

### Step 6 - Lower Index Reads and Checked Mutation-Adjacent Facts

- Lower index base before index operand.
- Copy the selected `CheckedTraitDispatch`, owning trait/method IDs, checked arguments, instantiated method type, ordered effects/resources, mutation/move masks, and temporary-cleanup requirements available at this stage.
- Do not select a concrete implementation or instance here; Stage 2.5 converts the checked recipe to explicit callable evidence.
- Cross-check read-index dispatch with the `MutateIndex` metadata already recorded on assignment items/places by Stage 2.3.
- Cover structural product/slice/ref indexing and explicit trait implementations, including move-only mutation temporaries.

Gate: Stage 3/5 can determine index argument evaluation, resource ordering, mutation behavior, and evidence inputs without consulting the source `IndexExpression` or type-checker maps.

After this step, update both plan files.

### Step 7 - Lower String Templates and Formatting Selections

- Preserve template part order and literal text exactly after source decoding.
- Lower interpolation expressions left-to-right.
- Store display/debug format, checked interpolation type, selected trait/method IDs, and formatting helper function identities.
- Diagnose unavailable formatting metadata during lowering rather than allowing name-based backend discovery.
- Keep helper instantiation and generated-artifact deduplication assigned to Stages 2.5 and 4.
- Add coverage for literal-only, mixed, multiple, nested, generic, debug, and early-exit interpolations.

Gate: a template's complete evaluation and formatting-selection plan is reconstructible from lowered IR, with no standard-function or standard-trait name lookup required.

After this step, update both plan files.

### Step 8 - Complete Validation, Coverage, and Transition Checks

Expand validation to check:

- no generic `Unlowered` expression remains;
- deferred families match only the syntax/categories owned by Stages 2.5 and 2.6;
- every child expression, block, pattern, place, symbol, trait, method, and function reference exists;
- occurrence keys and lookup entries agree without aliasing contextual defaults;
- product evaluation steps fill every final slot exactly as the checked plan permits;
- repeated-product counts and result shapes agree;
- access dereference paths and indices agree with checked source/result types;
- logical true indices are in range;
- breaks/continues have enclosing loops and match arms own their lowered patterns/bodies;
- index dispatch and formatting selections are complete enough for Stage 2.5;
- all lowered literal payloads are valid for their selected checked types.

Add a traversal that starts from every initializer and function body, visits every expression exactly through typed arena edges, and reports orphaned or multiply owned nodes except deliberately shared primary occurrences. Extend normalized repeated-lowering snapshots to all new expression payloads and add test-only comparisons with the corresponding checked side tables.

Gate: focused tests and the complete workspace suite pass; every accepted ordinary expression is concrete, every later-stage family is explicitly deferred, and no accepted runtime expression is represented only by the legacy payload.

After this step, mark Stage 2.4 complete in both plan files and identify Stage 2.5 as next.

## Testing Matrix

- Literals: all integer/float widths and range edges, decoded strings, escaped strings, empty strings, C strings, invalid/interior-NUL diagnostics.
- Names/access: locals, globals, mutable cells, captures, initialization checks, singletons, representations, products, refs, slices, scalar shortcuts, and callable deferral.
- Products: empty/singleton/fixed, named fields, designated order, positional spreads, named spreads, overrides, contextual defaults, generic defaults, repeated products, and divergent element evaluation.
- Blocks/coercions: nested blocks, tails, discarded droppable values, early return, `Never`, nominal/sum/string/ref/slice coercions.
- Control flow: `&&`, `||`, nested loops, break values, continue, divergent loops, sum/product/string matches, catch-alls, at-patterns, and branch-local ownership.
- Indexing: structural product/slice/ref reads, explicit implementations, effect resources, mutation/move signatures, and incomplete-dispatch diagnostics.
- Templates: literal-only, display/debug, multiple interpolations, generic values, nested expressions, and early exits.
- Coverage: one accepted fixture per owned expression family plus explicit fixtures proving Stage 2.5/2.6 nodes are deferred rather than silently unlowered.
- Determinism: lower representative programs repeatedly and compare normalized expression/ownership snapshots byte-for-byte.

Run after every completed step:

```text
cargo fmt --all -- --check
cargo check --workspace
cargo test -p staple-compiler lower::tests
git diff --check
```

Run at the final Stage 2.4 gate:

```text
cargo test --workspace
```

## Risks and Decisions

- Syntax ID alone is not a sufficient semantic occurrence key for contextual defaults. The implementation must solve this before lowering defaults, not patch over collisions afterward.
- Product source evaluation order and final field layout are different orders. The IR must represent both explicitly.
- Function/constructor/companion names look like ordinary names syntactically. Catalog classification, not syntax shape, decides whether Stage 2.4 completes or defers them.
- Index reads are trait calls, but their source-level evaluation/layout belongs here. Stage 2.4 stores the checked dispatch recipe; Stage 2.5 owns final callable/evidence classification.
- String templates currently discover formatting functions by name in code generation. Stage 2.4 must retain semantic selections earlier, while Stage 4 still owns generated helper discovery and deduplication.
- Match and loop lowering must preserve cleanup facts without embedding LLVM blocks, phi nodes, allocas, or instruction-level drop sequences.
- Resources appear in the high-level Stage 2.4 family list but have a dedicated Stage 2.6 contract. This plan deliberately defers `Resource` and `With` together so their lexical resource ordering is designed once.
- Coroutine construction/await, calls, closures, specialization instances, and helper catalogs remain out of scope.

## Deliverables

- Exhaustive occurrence-aware expression dispatcher with explicit later-stage deferrals.
- Concrete lowered nodes for every Stage 2.4 family.
- Owned product/default/spread evaluation plans and control-flow structures.
- Checked access, logical, match, index-dispatch, literal, and formatting metadata copied into lowered IR.
- Expanded validation, coverage traversal, deterministic snapshots, and transition comparisons.
- Updated `TYPED_LOWERING_PLAN.md` and `STAGE_2_LOWERING_BREAKDOWN.md` after every completed implementation step.
