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
