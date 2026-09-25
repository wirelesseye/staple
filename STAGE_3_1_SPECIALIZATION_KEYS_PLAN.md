# Stage 3.1 Plan: Specialization Inputs and Canonical Keys

## Goal and boundary

Define stable, structural identities for every source-function instance and the constructor/structural artifacts that Stage 3 may request. Inventory every current LLVM-time specialization route against the owned Stage 2 `LoweredProgram`, then implement and test canonical key values. Stage 3.1 does not build the worklist, resolve all generic substitutions, clone instance bodies, generate helper bodies, or migrate LLVM. Those belong to Stages 3.2–3.5, 4, and 5 respectively.

Stage 2 is complete at `aac6500`. Its `LoweredCall`, `LoweredCallableValue`, `LoweredClosureConstruction`, `TraitEvidence`, interpolation/index/place records, function catalog, and helper selections are the inputs. The legacy backend remains active for behavior comparison throughout Stage 3.

## Completion gate

- An exhaustive route inventory names each LLVM-time `ensure_function_specialization` caller and any adjacent constructor or structural artifact cache, the triggering source/implicit operation, and the owned lowered record that supplies its target, checked type, substitutions, evidence, and origin. Any backend-only discovery left for Stage 4 is identified explicitly.
- Structural type and effect keys cover every code-relevant `CheckedType` and `CheckedEffectSet` variant. Equivalent checked types with different display names, source/default metadata, or nominal representation expansion yield equal keys. Semantically different types, effects, ownership/ABI markers, and nominal IDs yield different keys.
- `InstanceKey` has a documented ordering and equality contract: `FunctionId` plus ordered relevant type/effect substitutions, with selected implementation evidence when it changes the body. Stage 3.1 may represent unresolved key inputs as a typed request; it must not silently invent a concrete instance for them.
- Constructor adapter and structural method requests have separate typed keys. Deterministic ID/name rules are specified and tested without depending on `HashMap` iteration, `Debug` output, or an unstable hash alone.
- Focused tests cover all distinctions listed below, and `cargo fmt --all -- --check`, `cargo check --workspace`, `cargo test --workspace`, and `git diff --check` pass after implementation.

## Current backend routes to inventory

Use `staple-compiler/src/codegen.rs` as the route checklist. Record each route in a table in the implementation notes, with a concrete lowered source and a test fixture; update the table if the code changes during Stage 3.1.

| Backend route | Current trigger | Stage 2 input or boundary |
| --- | --- | --- |
| `ensure_function_specialization` in generic direct-call compilation | A known generic callee, including same-function recursion | `LoweredCall.target = DirectFunction`, `function_type`, `substitutions`, `steps`, `origin`; `Current` environment marks recursion. |
| Function expressions, generic names, and companion-method selectors | A known function used as a value, possibly with captures | `LoweredCallableValue` and `LoweredClosureConstruction` supply target, complete checked callable type, captures, environment mode, substitutions, and origin. |
| Selected trait implementation methods | Trait call or method value after selection | `TraitEvidence::ExplicitImplementation` or `DeclaredBound`, `LoweredCallableTarget::TraitImplementation`, catalog implementation/method IDs, and site substitutions. Stage 3.2 resolves declared bounds. |
| String-template formatter `new`, `write`, and `finish` | Formatting helper calls emitted by LLVM | `LoweredStringFormatting` gives selected `FunctionId`s; `LoweredStringTemplate` and interpolation evidence give origin and selected formatting method. Distinguish source-function instances from Stage 4 generated formatting helpers. |
| Structural trait methods | Debug, indexing, indexed mutation, iterator operations, and formatting evidence | `TraitEvidence::Structural` on calls, callable values, index/place/interpolation sites. The generated structural body is a Stage 4 artifact request. |
| Constructor closure adapter | Constructor used as a callable value | `LoweredCallableTarget::Constructor`, `LoweredCallableAdapter::Constructor`, checked function type, nominal/symbol IDs. It is a distinct artifact from the constructor call itself. |
| Nested/captured closures and implicit thunks | Function values, default/thunk arguments, reactive callbacks, derived evaluators, coroutine body links | Closure construction and function catalog; ordered call steps/default occurrences, reactive/coroutine records. Confirm whether each reaches a generic specialization, an eager nongeneric function, or a Stage 4 helper. |

Also trace the eager nongeneric-function loop and `compile_queued_specializations`: all nongeneric functions and implicit thunks except coroutine body thunks are compiled eagerly; module initializers are compiled before queued specializations. Capture this as later worklist-root behavior, not as a reason to create Stage 3 instances during this step. Inspect indirect calls and extern/intrinsic routes to document why they do not request a known `FunctionId` instance at the call site. Search every call to `ensure_function_specialization`, `ensure_constructor_adapter`, and `structural_trait_method_code` before calling the inventory complete.

The current ordinary cache is `(FunctionId, format!("{function_type:?}"))`; it infers substitutions from the template and complete checked function type, merges `active_type_substitutions`, and names code with a hash of that string. The constructor cache uses `(SymbolId, Debug function type)` and the structural cache uses `(StructuralTraitMethod, Debug arguments)` with an insertion-count suffix. These are migration baselines, not key definitions.

## Key contract to establish

### Canonical type and effect values

Use typed, owned, `Eq`/`Hash`/ordered structural values (or an equally explicit canonical byte encoding) rather than formatting checked types. Visit all `CheckedType` variants exhaustively: primitive and literal types; `Ref`, `Slice`, `Buffer`, `Array` and its count; `CPointer`; products and sums; functions; declared parameters; and `TypeConstructor`, `Opaque`, and `Distinct` nominal forms. A nominal node uses its semantic `TypeId`, nominal kind, and canonical arguments. Do not descend into `Distinct.representation` or expand a recursive nominal representation. Keep the nominal kind when it affects semantics; decide equivalence of `Opaque` and expanded forms against checker equality rather than by display spelling.

Products retain field names, field order, element types, and variadic flag, while dropping contextual element defaults. Sums retain alternative order and types; do not perform new type normalization here. Functions retain parameter/result types, parameter style, ordered mutation and move markers, and the full effect row. Drop `CheckedFunctionType.default` and other source expressions from the key. Effects retain resource order, each value type and mutability, state access mode, and a declared effect-variable ID in a template-level key. A concrete instance key requires that variable to be substituted. Preserve literal payloads and repeated-product/array counts when they change layout or body behavior. Use semantic parameter IDs, never parameter names or `sized` display metadata, as identity; validate any constraint that must hold at instantiation separately.

Reject `Inferred` and `Error` when forming a concrete key. `Never` is a valid checked type where the checker permits it; distinguish it structurally. Do not use source spans, resolved display names, contextual defaults, or the expanded representation of a nominal type as equality inputs. Document any intentionally excluded metadata with the corresponding checker invariant so a future new `CheckedType` variant forces an explicit decision.

### Function instance key

Define `InstanceKey` as `FunctionId` plus ordered `(TypeParameterId, canonical type-or-effect value)` entries for parameters relevant to the function's signature, body, captures, layout, or evidence. Add a canonical selected-evidence component only where two selections under the same substitutions would emit different code; use semantic `TraitId`, `TraitMethodId`, implementation ID or structural method kind, and canonical arguments. Keep distinct implementation IDs distinct even when their method signatures happen to match. A function with no relevant parameters or evidence has one key.

Stage 3.1 defines the shape and comparison rules. Stage 3.2 determines the free/relevant parameter set, composes `CallSubstitutions` with the enclosing environment, normalizes type-encoded effect substitutions, and resolves `DeclaredBound` evidence. Keep requests with unresolved parameters/evidence separate from interned concrete keys until that resolution succeeds. Order entries by `TypeParameterId` and enforce no duplicate/conflicting entry; the source order of `HashMap` traversal must never enter equality or naming.

### Artifact request keys

Define a constructor-adapter request key from constructor semantic identity (`SymbolId` and/or `TypeId` as justified by the catalog), the concrete canonical callable type, and adapter kind. Define a structural-method request key from `StructuralTraitMethod`, `TraitId`, `TraitMethodId`, completed canonical trait arguments, and concrete callable type when it can affect the ABI/body. Keep source-function, constructor-adapter, and structural-method namespaces distinct. Stage 4 materializes their bodies and discovers dependencies inside them; Stage 3 only records typed requests. Verify whether the current structural cache omits method/function-type information safely before retaining that narrower identity.

## Implementation sequence

### Step 1 — Freeze the route/input matrix

- Read every backend specialization and adjacent generated-artifact call site, including direct calls, callable names/selectors, anonymous closures, explicit/structural trait paths, string formatting, constructors, and recursion. Record whether the route discovers a source function, an adapter, a structural body, or a later helper.
- Trace each to the Stage 2 target, `CallSubstitutions`, checked function type, `TraitEvidence`, captures, and `Origin`. Note missing owned data as a concrete Stage 3.1 input gap rather than falling back to `TypedModule` or source AST in the new key layer.
- Include a negative matrix for indirect closure, external, intrinsic, and ordinary constructor calls, plus eager nongeneric roots, so later worklist traversal does not over-specialize them.

Gate: every backend entry point is accounted for, and each new request can be described using owned semantic IDs and checked values.

### Step 2 — Implement canonical structural values

- Add a private key module or similarly focused types. Make conversion exhaustive over type/effect variants; choose an explicit, stable ordering for nested vectors and optional fields.
- Keep nominal types compact by `TypeId` and arguments; retain product/variadic/sum/function shape and mutation/move/effect distinctions. Normalize checked effect values at the key boundary only after Stage 3.2 has resolved them; template-key conversion may retain parameter IDs.
- Make malformed concrete inputs return a source-based diagnostic using the requesting record's `Origin`, not a panic or a compiler-only span.

Gate: equality tests prove same semantics deduplicate despite different names/defaults/representation payloads, while nominal identity, counts, field order/names, variadic shape, parameter style, ownership masks, resource order/mutability, state effects, and literal values separate.

### Step 3 — Define instance and artifact request keys

- Implement typed `InstanceKey` and typed constructor/structural request keys with explicit namespace separation. Use ordered substitution entries and selected-evidence IDs; document which equality inputs are provisional until Stage 3.2 resolves them.
- Add constructors that reject duplicate parameter IDs, conflicting type/effect entries, unresolved concrete values, and evidence that is still a declared or negative obligation. Do not conflate an unresolved request with a concrete key.
- Prove a generic function whose callable signature is identical under two outer substitutions can still receive different keys when captures/body/evidence depend on those substitutions; defer collection of that relevance to Stage 3.2.

Gate: key tests distinguish two nominal IDs with the same display name, two effect rows with different resources or state access, same-signature captured closures with different outer arguments, and two selected implementations.

### Step 4 — Specify deterministic identity and handoff

- Define key comparison and deterministic catalog insertion order for Stage 3.3. Prefer an append-only ordinal assigned in root/first-discovery order with collision-checked names, or an explicitly versioned canonical encoding with collision detection. Neither `DefaultHasher` output nor a `Debug` string alone is a stable emitted identity.
- Add repeat-run tests for canonical encoding/comparison and artifact namespace separation. Record how Stage 3.3 will reserve an ID before visiting a body so self-recursion and mutual recursion converge.
- Update `STAGE_3_SPECIALIZATION_BREAKDOWN.md` and `TYPED_LOWERING_PLAN.md` with actual completion evidence and the Stage 3.2 handoff only after the gate passes.

Gate: identical inputs produce identical typed keys and planned names/ordering across repeated runs; hash collisions cannot silently alias two semantic keys.

## Test matrix and verification

Use small checked/lowered fixtures for: nominal IDs and recursive nominal representations; product names/order/default changes; variadic and repeated/array counts; function parameter style and ordered move/mutation markers; empty and nonempty effect rows with resource mutability/order and state modes; type and effect parameters; generic closure captures whose callable signatures match; selected explicit implementations versus structural methods; constructor adapters; and formatting selections. Include a test that a recursive nominal key stays compact, plus a test that an unresolved effect or `Inferred`/`Error` value fails at the concrete-key boundary with the requesting origin.

Run the full workspace checks listed in the completion gate. No LLVM migration is expected in Stage 3.1, so existing compile/run and LLVM regression tests must continue to pass through the legacy backend. The deliverable is the route matrix, typed key contract and implementation, focused tests, and a documented Stage 3.2 input boundary.

## Implementation Notes

### Step 1 — Frozen backend route/input matrix

Inventory method: every call site of `ensure_function_specialization` (nine), `ensure_constructor_adapter` (one), and `structural_trait_method_code` (one, reached through the `trait_method_code` selector), plus the adjacent generated-artifact caches (`specialized_functions`, `constructor_codes`, `structural_trait_codes`, `gc_finalizers`, `coroutine_codes`), was read in `staple-compiler/src/codegen.rs`. Callers of the shared `trait_method_code` selector were read too, since they all funnel into the same explicit-implementation or structural-method decision. Line references are against the Stage 3.1 starting commit `595712a`.

#### Specialization and artifact routes

| # | Backend route | Current trigger | Discovered artifact | Owned Stage 2 input | Fixture/test |
| --- | --- | --- | --- | --- | --- |
| 1 | `compile_call_expression` generic branch, `ensure_function_specialization` (`codegen.rs:6858`) | A known generic callee without captures used directly, including same-function recursion (`environment.function_id == Some(function_id)` selects `Current`) | Source-function instance | `LoweredCall.target = DirectFunction { function, environment }`, `function_type`, `substitutions`, `steps`, `origin` | `ordinary_direct_indirect_external_and_intrinsic_calls_lower`, `call_facts_agree_with_checked_function_types` |
| 2 | `Expression::Function` value branch, `ensure_function_specialization` (`codegen.rs:3103`) | A generic function literal used as a first-class value, with or without captures | Source-function instance plus closure construction | `LoweredCallableValue.target = DirectFunction`, `function_type`, `closure: LoweredClosureConstruction { captures, environment, adapter, substitutions }`, `substitutions`, `origin` | `function_values_record_targets_adapters_and_closure_plans` |
| 3 | `Expression::Access` companion-selector branch, `ensure_function_specialization` (`codegen.rs:3357`) | `receiver^method` whose selector symbol names a generic function | Source-function instance plus `Stored` closure reuse | `LoweredCallableValue` for the declared/generic function route with `DirectFunction` target, `function_type`, `closure`, `substitutions`, `origin` | `function_values_record_targets_adapters_and_closure_plans` |
| 4 | `Expression::Name` binding branch, `ensure_function_specialization` (`codegen.rs:3525`) | A generic function bound to a name and used as a value | Source-function instance plus closure construction | Same as row 2: `LoweredCallableValue` with `DirectFunction` target and its closure plan | Same as row 2 |
| 5 | `trait_method_code` explicit-implementation branch, `ensure_function_specialization` (`codegen.rs:3658-3666`) | A selected explicit implementation whose method function is still generic at the current substitutions; reached from trait calls (`6778`), trait dispatch expressions (`3056`), trait-method values (`5561`), `Index` reads (`6265`), `MutateIndex` assignment (`2581`), interpolation formatting (`4041`), nested Debug calls (`3817`, `3885`), Deref index/mutate bodies (`4315`, `4369`), and buffer clone (`13193`) | Source-function instance | `LoweredCall.target = TraitImplementation` / `LoweredCallableValue` plus `TraitEvidence::ExplicitImplementation { trait_id, implementation, method, function, arguments }`, `function_type`, `substitutions`, `origin` | `trait_calls_index_mutation_and_interpolations_carry_evidence` |
| 6 | `structural_trait_method_code` (`codegen.rs:3674`), selected by `trait_method_code` (`3671`) | No explicit implementation exists for the completed trait arguments; the compiler generates the structural body | Structural-method artifact request (body is Stage 4) | `TraitEvidence::Structural { trait_id, method, structural, arguments }` on calls, callable values, `Index` reads, `MutateIndex` assignments, and interpolations | `trait_calls_index_mutation_and_interpolations_carry_evidence` |
| 7 | `compile_string_template` constructor call, `ensure_function_specialization` (`codegen.rs:3982`) | Lowering a string template selects `Formatter.new` | Source-function instance (a Stage 4 formatting-helper selection) | `LoweredStringFormatting.constructor`, `LoweredStringTemplate` parts | `string_templates_record_parts_and_formatting_selections` |
| 8 | `compile_formatter_write_literal`, `ensure_function_specialization` (`codegen.rs:3937`) | Compiler-generated structural `Debug` bodies write literal punctuation | Source-function instance requested from inside a generated artifact body | `LoweredStringFormatting.write` at source template sites; the structural body itself is a Stage 4 artifact request | `string_templates_record_parts_and_formatting_selections`, `trait_calls_index_mutation_and_interpolations_carry_evidence` |
| 9 | `compile_string_template` finish call, `ensure_function_specialization` (`codegen.rs:4082`) | Lowering a string template selects `Formatter.finish` | Source-function instance (a Stage 4 formatting-helper selection) | `LoweredStringFormatting.finish` | Same as row 7 |
| 10 | Interpolation dispatch through `trait_method_code` (`codegen.rs:4041`) | Each `{value}` interpolation selects `Display` or `Debug` for the checked value type | Explicit-implementation instance or structural-method artifact | `LoweredInterpolation { trait_id, method, value_type, evidence }`, `LoweredStringFormatting` | `string_templates_record_parts_and_formatting_selections` |
| 11 | `Expression::Name` constructor branch, `ensure_constructor_adapter` (`codegen.rs:3498`) | A nominal constructor used as a callable value | Constructor-adapter artifact (distinct from the constructor call) | `LoweredCallableValue.target = Constructor { symbol, type_id, recursive }`, `adapter = Constructor`, `function_type`, `origin` | `constructor_calls_and_values_record_explicit_targets` |
| 12 | `Expression::Call` constructor branch (`codegen.rs:3280-3317`) | A nominal constructor invoked directly, including managed `Ref` construction | None; direct value construction | `LoweredCall.target = Constructor { symbol, type_id, recursive }` | `constructor_calls_and_values_record_explicit_targets` |

#### Negative matrix (must not become Stage 3 instances)

| Backend route | Current behavior | Owned Stage 2 input |
| --- | --- | --- |
| Indirect closure call and closure fallback (`compile_call_expression` tail `codegen.rs:6982-7037`, `compile_indirect_call_value` `7040`) | Invoked through the closure code pointer; no `FunctionId` instance requested at the call site | `LoweredCall.target = IndirectClosure { callee }`, `callee: Some(..)`, `function_type`; the callable value record at the construction site is what may identify a known function |
| External call (`globals` branch `codegen.rs:6890-6980`) | Calls the declared external function directly | `LoweredCall.target = ExternalFunction { symbol }`; `LoweredSymbol.external` |
| Intrinsic call (`codegen.rs:6818`, `compile_intrinsic_call` `7142`) | Lowered inline or to runtime entry points; `BufferClone` builds helpers through `compile_buffer_clone` (`7208`) | `LoweredCall.target = Intrinsic { symbol, intrinsic }`; reactive/coroutine intrinsic routes are explicit Stage 2.6 records |
| Compiler-helper category | Reserved category; no source call produces it yet | `LoweredCallableTarget::CompilerHelper { function }` staged for Stage 4 helper selection |
| Eager nongeneric function and implicit-thunk loop (`codegen.rs:388-404`, declared by `declare_functions` `779`) | Every function/thunk without a type parameter, except coroutine body thunks, is compiled eagerly before initializers | `LoweredFunction.signature`, `class`, `origin`; one key per function |
| Coroutine body thunk (`is_coroutine_body_thunk` `9164`, `ensure_coroutine_codes` `9343`, `compile_coro_expression` `9880`) | Demand-driven `resume`/`cleanup` pair keyed by body syntax, never in the eager loop | `LoweredCoroutinePlan { body_syntax, thunk, captures, deferred_effects, resume_points, .. }`, `LoweredCoro`; Stage 4 owns the generated pair |
| Closure and buffer finalizers (`ensure_closure_finalizer` `11103`, `ensure_buffer_finalizer` `13373`) | Generated layout-dependent GC finalizers cached by debug-string keys | Closure layout/captures and buffer element types on Stage 2 records; Stage 4 artifact requests |

#### Root and queue order (later worklist roots, not Stage 3.1 instances)

- Module initializers are declared per module (`codegen.rs:386`) and compiled in program initialization order (`compile_module_initializers` `1753`); `main` calls them in `initialization_order()` before returning (`1717-1726`).
- Eager nongeneric functions and implicit thunks are compiled in `functions()` followed by `implicit_thunks()` order (`388-404`), while coroutine body thunks are skipped.
- `compile_queued_specializations` (`966`) drains the specialization queue after module initializers, so generic instances are appended in first-request order.

#### Owned-input completeness

- Every specialized route above can be described with owned semantic IDs and checked values: `FunctionId`, `SymbolId`, `TypeId`, `TraitId`, `TraitMethodId`, `LoweredTraitImplementationId`, `StructuralTraitMethod`, `CheckedType`/`CheckedFunctionType`/`CheckedEffectSet`, `CallSubstitutions`, `TraitEvidence`, `LoweredCapture`/`LoweredClosureConstruction`, and `Origin`.
- Input gaps recorded for later stages, not worked around in the key layer:
  - The backend infers missing substitutions from the template plus the complete checked callable type (`ensure_function_specialization` `875-897`) and merges `active_type_substitutions`; Stage 3.2 must compose `CallSubstitutions` with the enclosing environment instead. Stage 3.1 keys represent the site recipe without inventing the missing entries.
  - The backend computes instantiated trait-method types from `TypedModule` (`instantiated_trait_method_type` in `trait_method_code`); Stage 2 owns the template type (`LoweredTraitMethodMetadata.value_type`) and the site `function_type`, which are sufficient once Stage 3.2 completes the arguments.
  - Structural `Debug` bodies re-select `Formatter.write` and the standard `Debug` trait by name (`codegen.rs:3924`, `3794`); those generated-body helper selections are Stage 4 artifact dependencies and must not be re-discovered by the Stage 3 key layer.
  - Coroutine `resume`/`cleanup` pairs and GC finalizers are generated layout artifacts, not source-function instances; Stage 4 owns their typed requests.

**Step 1 gate:** every `ensure_function_specialization`, `ensure_constructor_adapter`, and `structural_trait_method_code` entry point is accounted for above, each request is describable from owned Stage 2 records, and the negative matrix keeps indirect, external, intrinsic, constructor-call, eager-root, and coroutine routes out of instance creation.

### Step 2 — Canonical structural values (complete)

- Added the private `staple-compiler/src/specialization.rs` key module with `CanonicalType`, `CanonicalFunctionType`, `CanonicalEffectSet`, `CanonicalResource`, `CanonicalStateEffect`, `CanonicalProductElement`, `CanonicalMutation`, and `CanonicalNominalKind`. All values are owned, typed, and `Eq`/`Hash`; no checked type is formatted into a key.
- Conversion is an exhaustive match over every `CheckedType` variant. `template` retains declared parameter and effect-variable IDs for Stage 3.2; `concrete` rejects them with a source diagnostic at the requesting record's `Origin`. `Inferred` and `Error` are rejected in both modes.
- Nominal nodes keep semantic `TypeId`, nominal kind (`TypeConstructor`/`Opaque`/`Distinct`), and canonical arguments; display names and `Distinct.representation` are dropped, so recursive or expanded representations stay compact. Product names/order/variadic flag, sum order, array/repeated counts, literal payloads, parameter style, ordered mutation/move masks, resource order/mutability, and state access mode all participate in equality.
- Deliberately excluded metadata, with the checker invariant that makes it safe: `CheckedTypeElement.default` and `CheckedFunctionParameterDefault` are source expressions the checker ignores in type equality and codegen never reads from a signature; `Parameter.name`/`sized` are display/validation metadata, never identity; effect values are preserved in checked row order because Stage 3.2 performs substitution and normalization before concrete keying; `Distinct.representation` is expanded structure whose semantics are carried by the nominal ID plus arguments.
- Focused tests prove same-semantics dedup (different names, different expanded representations, different contextual defaults) and separation of nominal IDs/kinds/arguments, product names/order/variadic shapes, sum order, literal and repeated counts, mutation/move order, resource order/mutability, state modes, and empty versus nonempty effect rows. A 64-deep nominal representation chain converts to the same bounded key as its shallow form, and unresolved parameters/effects/placeholders fail with the requesting span.
- Verified with `cargo fmt --all -- --check`, `cargo check --workspace`, and the focused `specialization` tests (all passing).

### Step 3 — Instance and artifact request keys (complete)

- Added `InstanceKey` (`FunctionId` + ordered `InstanceSubstitution` entries + optional `CanonicalEvidence`). Substitution entries are stably sorted by `TypeParameterId`; duplicate same-kind entries, a parameter carrying both a type and an effect entry, and any entry whose value still contains a declared type parameter or effect variable are rejected through `InstanceKeyError`. Which parameters are *relevant* remains Stage 3.2's collection: the key compares whatever entries callers supply.
- Added `InstanceRequest`, holding the unmodified Stage 2 `CallSubstitutions` and `TraitEvidence` plus the requesting `Origin`. `resolve()` converts substitutions and evidence with the concrete canonical converters and maps key errors to a source diagnostic at that origin. `TraitEvidence::DeclaredBound` and `TraitEvidence::RejectedImplementation` never resolve: declared bounds belong to Stage 3.2, and negative implementations never become instances. Unresolved requests therefore cannot be mistaken for concrete keys.
- Evidence identity uses semantic `TraitId`/`TraitMethodId`, the selected method `FunctionId` (unique per implementation method, which keeps two implementations distinct even when their signatures match), structural method kind, and canonical arguments. `LoweredTraitImplementationId` is not part of the key because it is an arena position (and not constructible outside `lower.rs`); the selected method function already distinguishes implementations semantically.
- Added namespaced artifact keys: `ConstructorAdapterKey` (constructor `SymbolId` + nominal `TypeId` + canonical adapter kind + concrete callable type), `StructuralMethodKey` (structural method kind + `TraitId` + `TraitMethodId` + completed canonical arguments + concrete callable type), the `ArtifactRequestKey` enum that separates the two artifact namespaces, and `SpecializationKey` that separates source-function instances from artifacts. The legacy structural cache's `(StructuralTraitMethod, Debug arguments)` identity was checked: for the seven current methods the trait, method, and callable type are derivable from kind plus arguments, but the request key keeps all of them explicit so Stage 4 never depends on that derivability.
- Focused tests prove substitution dedup regardless of input order; duplicate/conflict/unresolved rejection; request resolution and origin-bearing failure for declared bounds, negative evidence, unresolved substitutions, and unresolved evidence arguments; that identical callable signatures still separate when outer substitutions differ; that two selected implementations, explicit versus structural selections, and different structural arguments all separate; and that artifact keys are namespaced, adapter/symbol-sensitive, and reject unresolved callable types/arguments.
- Verified with `cargo fmt --all -- --check`, `cargo check --workspace`, and the focused `specialization` tests (12 passing).
