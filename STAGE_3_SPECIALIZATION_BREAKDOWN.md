# Stage 3 Breakdown: Structural Instances and Specialization Worklist

## Status and Goal

**Status:** Not started. Stage 2 is complete at `aac6500`.

Build the concrete function-instance graph from the owned Stage 2 `LoweredProgram` before LLVM emission. Each reachable function template gets one instance per distinct code-relevant substitution and selected evidence. Stage 3 records instance references on lowered calls and callable values, substitutes their bodies, and validates the graph. Stage 4 extends discovery to compiler-generated helper bodies; Stage 5 makes LLVM consume the graph and removes its old specialization queue. The legacy backend bridge stays active until Stage 5.

## Instance Contract

- Add an internal `FunctionInstanceId`, `InstanceKey`, `LoweredFunctionInstance`, and append-only instance catalog to `LoweredProgram`. An instance retains its template `FunctionId`, source origin, fully substituted signature/body, selected evidence, dependencies, and generated symbol identity.
- A function instance contains only concrete runtime types and effects. A declared type/effect parameter may remain in a template, but never in an emitted instance. Reject `Inferred`, `Error`, unresolved effect variables, and unresolved trait obligations at an instance boundary with a source-based diagnostic.
- Keep template arenas immutable. Instantiate into separate instance-owned nodes, preserving Stage 2 evaluation order, source origins, ownership facts, and links among expressions, items, patterns, places, calls, resources, reactive operations, and coroutine plans. Reuse only immutable declaration metadata whose meaning does not vary with the substitution.
- Represent a reference to a known concrete function by `FunctionInstanceId`; indirect closure calls keep their indirect target. Calls to externs and intrinsics keep their existing semantic targets.
- The worklist has a deterministic root order, dependency traversal order, insertion order, and output order. Intern a key and reserve its ID before visiting its body so self-recursion and mutual recursion converge.

## Stage 3.1 - Inventory Specialization Inputs and Define Canonical Keys

- Trace all current LLVM-time `ensure_function_specialization` routes, including direct generic calls, function-valued names and selectors, anonymous and captured closures, selected trait methods, formatting helpers, and recursive calls. Map each route to the Stage 2 call, callable-value, trait-evidence, or helper-selection record that supplies it.
- Define a canonical structural type/effect key independent of `Debug` output. Use semantic nominal IDs and type arguments; retain product field names, order, variadic shape, function parameter style, mutation/move masks, effect resources and access modes, state effects, sums, repeated-product counts, and other ABI or body-relevant structure. Exclude display names, source spans, and contextual defaults. Avoid recursively expanding nominal representations in keys.
- Define `InstanceKey` as the function `FunctionId` plus ordered `(TypeParameterId, canonical value)` substitutions for every parameter on which its signature, body, captures, layout, or selected evidence depends. Include canonical selected implementation evidence when it changes the generated body. A concrete function with no relevant substitutions has one key.
- Provide structural keys for constructor closure adapters and structural trait methods, using their semantic target and concrete function/trait arguments. Reserve artifact requests in this stage; Stage 4 completes their generated bodies and dependency discovery.
- Use stable key comparison and a deterministic symbol-name encoding or collision-checked ordinal. Hash-map iteration order and `Debug` strings must not affect emitted identities.

**Gate:** Key tests distinguish nominal identities, effect rows, ownership markers, capture-dependent outer substitutions, and selected implementations; equivalent checked types with different display/default metadata deduplicate.

> **Complexity note:** Canonicalizing recursive nominal types and effect substitutions without expanding representations may need a focused design pass.

## Stage 3.2 - Resolve Substitutions and Relevant Parameters

- Collect the free type/effect parameters of each function template from its signature, body metadata, captures, declared bounds, and trait-evidence recipes. Include enclosing parameters used by nested closures even when absent from the nested function signature. Exclude outer parameters that cannot affect the instance.
- Compose Stage 2 `CallSubstitutions` with the enclosing instance environment. Infer any missing declared parameters from the complete checked callable type, including result-only parameters and effect rows; never guess an unconstrained parameter.
- Apply substitutions transitively and in parameter-ID order. Normalize effect substitutions out of the checker's type-encoded representation before keying. Detect conflicting mappings and substitution cycles.
- Resolve `TraitEvidence::DeclaredBound` under the concrete substitution using its recorded prerequisites and checked implementation choices. Preserve explicit and structural selections; reject a negative implementation or an unsatisfied obligation. Do not perform runtime implementation search.
- Keep same-function recursive calls on the current key, enforcing the existing prohibition on polymorphic recursion.

**Gate:** Unit tests cover result-only parameters, empty and nonempty effect rows, nested closure captures, irrelevant outer parameters, conditional trait implementations, prerequisites, structural evidence, and same-key recursion.

> **Complexity note:** Free-parameter collection and evidence resolution are the highest-risk parts of this stage; plan them separately during implementation if their invariants need more detail.

## Stage 3.3 - Build the Deterministic Worklist

- Seed module initializer bodies in program initialization order and seed the same nongeneric functions and implicit thunks that the current backend emits eagerly. Keep coroutine body thunks demand-driven as they are today. Do not seed unused generic templates.
- Traverse each root and discovered instance in lowered evaluation order. Follow direct calls, function values, selected trait implementation methods, nested closures, implicit thunk arguments, reactive callbacks, derived evaluators, coroutine body links, and default expressions. Preserve indirect calls as indirect unless their construction site identifies a known function.
- Intern each requested `InstanceKey` before processing its body; enqueue only a newly interned key. Record typed dependency edges and the source origin that requested each instance.
- Record constructor-adapter and structural-method requests using their typed keys. The Stage 3 graph may carry unresolved compiler-helper artifact requests; Stage 4 closes those generated-helper dependencies before LLVM migration.
- Define stable emission order as root order followed by first-discovery worklist order. Names and IDs must be repeatable across runs of the same program.

**Gate:** Repeated uses deduplicate, unused generic bodies are absent, nested generic calls discover their dependencies, recursion terminates, and repeated lowering yields the same instance graph.

## Stage 3.4 - Materialize Concrete Instance Bodies

- Clone each reachable template into an instance-owned body, substituting all checked types, effect rows, bounds, trait arguments, aggregate/count metadata, capture types, resource requirements, and coroutine frame metadata.
- Replace known-function call and callable-value targets with their interned instance references; retain the existing direct versus indirect call decision and closure-environment mode. Preserve ABI argument order and move/mutation/drop facts.
- Resolve each trait-dependent call, index operation, indexed assignment, and interpolation to its concrete explicit or structural evidence. Store the selected method/function reference or typed structural request on the instance node.
- Recompute only facts that depend on the concrete substitution, such as copy/drop requirements and substituted aggregate shape, using the checked metadata and existing rules. Do not infer new source-language types or change the source evaluation order.
- Keep nominal recursive representation templates compact and refer to them by semantic ID plus concrete arguments.

**Gate:** Every instance body is independently traversable without consulting a generic template or a `TypedModule` side table for type arguments, call targets, or trait selection.

> **Complexity note:** Substitution touches every Stage 2 arena family. Implementation may need smaller plans by expression, ownership/resource, and reactive/coroutine families.

## Stage 3.5 - Validate the Graph and Compare with Current Emission

- Validate key/catalog agreement, unique IDs and names, complete concrete substitutions, all instance-body references, evidence selection, dependency edges, closure capture substitutions, recursive back-edges, and absence of template-only values in emitted instances.
- Add focused fixtures for direct and indirect calls, result-only generics, curried layers, generic captures with the same callable signature, constructor adapters, structural traits, conditional implementations, functional dependencies, effect-polymorphic callbacks, repeated products, defaults, cross-module calls, and coroutine/reactive thunks.
- Add a test-only transition comparison for representative programs: compare the new graph's concrete function identities and substitutions with the legacy LLVM specialization queue and verify that all currently emitted specializations have a matching new instance. Account separately for Stage 4 helper artifacts.
- Run `cargo fmt --all -- --check`, `cargo check --workspace`, `cargo test --workspace`, and `git diff --check`. Exercise CLI LLVM, object, and run paths with the branch worktree's standard library.
- Update [TYPED_LOWERING_PLAN.md](TYPED_LOWERING_PLAN.md) and this file after each completed step, recording what passed and what remains.

**Gate:** The worklist reaches a fixed point, every emitted instance is concrete and validated, no unused generic template becomes an instance, repeated runs are deterministic, and the existing backend regression suite still passes.

## Stage Boundary and Definition of Done

- Stage 3 delivers a complete graph for reachable source and checked-IR function instances, plus typed constructor/structural/helper requests. Stage 4 discovers and materializes dependencies inside generated helpers and verifies the final artifact catalog is closed.
- Stage 3 does not switch LLVM to the new graph or remove its legacy queue; Stage 5 performs that migration after Stage 4.
- No new source syntax, polymorphic-value runtime ABI, dictionary passing, descriptor construction, or runtime specialization is introduced.
- Mark Stage 3 complete in the main plan only when the graph, key, substitution, recursion, evidence, determinism, and transition tests pass; then identify Stage 4 as next.
