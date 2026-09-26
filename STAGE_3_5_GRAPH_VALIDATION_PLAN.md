# Stage 3.5 Plan: Validate the Graph and Compare with Current Emission

**Status:** Complete. Stage 3.4 is implemented through `011757b`; Stage 3.5 Steps 1-4 are implemented, validated, and recorded below. Stage 4 (compiler-generated artifacts) is next.

## Goal and boundary

Prove the Stage 3.3 worklist and its Stage 3.4 concrete bodies are a closed, deterministic fixed point, and show that every specialization the legacy LLVM backend currently emits is represented by the new graph. Stage 3.5 adds validation and tests only: the legacy backend and the private `TypedModule` bridge stay operational until Stage 5.

Stage 3.5 does not discover instances, intern keys, generate Stage 4 helper bodies, or migrate emission. It audits what Stages 3.1-3.4 already installed and compares legacy emission against it in tests.

## Completion gate

- Graph-level validation confirms catalog/arena agreement, unique ordinals and names, complete and canonical concrete substitutions for every instance environment, resolved evidence for every instance key, dependency and recursive back-edge integrity, and the absence of template-only checked values in emitted instance bodies.
- Focused fixtures cover direct and indirect calls, result-only generics, curried layers, generic captures with the same callable signature, constructor adapters, structural traits, conditional implementations, functional dependencies, effect-polymorphic callbacks, repeated products, defaults, cross-module calls, and coroutine/reactive thunks; every fixture lowers, materializes, and validates through `Lowerer::lower`.
- A test-only transition comparison compiles representative programs with the legacy backend, collects its specialization queue and its generated constructor/structural discoveries, and proves every legacy source-function specialization matches an interned instance by template plus concrete substitutions, with constructor adapters and structural methods accounted for as Stage 4 artifact requests.
- `cargo fmt --all -- --check`, `cargo check --workspace`, `cargo test --workspace`, and `git diff --check` pass; the CLI `--emit llvm`, `--emit object`, and `run` paths succeed with the worktree standard library.

## Implementation sequence

### Step 1 — Graph-level validator

- Add a private `lower/graph_validation.rs` child module with `LoweredProgram::validate_specialization_graph`, run in `Lowerer::lower` after the Stage 3.4 body validator.
- Rebuild each instance's `InstanceKey` from its pruned `SubstitutionEnvironment` and resolved evidence with the Stage 3.1 canonical converters; a mismatch, a leftover declared parameter or effect variable, or a checker placeholder diagnoses at the instance origin.
- Check that each environment holds exactly the relevant parameters, that planned names are non-empty and unique, and that dependency edges keep key identity: a recursive back-edge resolves to the owner instance, and an edge to another instance never carries the owner's key.
- Walk every emitted body's call, callable-value, and closure construction records: substitution values must be free of declared type parameters and effect variables, closure capture order must match the template capture order, and no site or evidence table may retain a declared-bound or rejected recipe.

**Gate:** A corrupted environment, key, name plan, dependency edge, or template-only body value is reported; every valid program reaches `Lowerer::lower` success.

**Step 1 — complete.** `lower/graph_validation.rs` owns `validate_specialization_graph`, called by `Lowerer::lower` after `validate_instance_bodies` and `validate_specializations`. It rebuilds every instance key from the pruned environment and resolved evidence with `CanonicalType::concrete`/`CanonicalEffectSet::concrete`/`canonical_evidence`, checks the environment holds exactly the relevant parameters, checks planned and stored names are non-empty and unique across both key families, checks dependency edges keep key identity (a recursive back-edge targets the owner; no edge to another instance reuses the owner's key), and walks every body's call, callable-value, and closure construction records for declared parameters, effect variables, closure capture order against the template, missing dependency targets, and unresolved evidence recipes. Focused tests corrupt an environment, evidence, a name, a dependency target, and a body substitution and prove each is reported.

### Step 2 — Focused graph fixtures

- Add fixtures over the full `Lowerer::lower` pipeline for: direct and indirect calls, result-only generics, curried layers and repeated products, generic captures with the same callable signature, constructor adapters, structural traits, conditional implementations, functional dependencies, effect-polymorphic callbacks, defaults, cross-module calls, and coroutine/reactive thunks.
- Assert graph facts, not just successful lowering: which route each site binds, which instances exist and deduplicate, which artifacts are requested, and that no unused generic template becomes an instance.
- Prove the fixed point: materialization adds no instance or dependency, a second `build_specialization_worklist` yields the same graph, and identical requests share one instance.

**Gate:** Every listed scenario lowers, materializes, validates, and yields the expected route/binding/instance shape.

**Step 2 — complete.** Fixtures live with the validator and exercise the whole `Lowerer::lower` path: direct and indirect calls keep instance versus route bindings, result-only generics infer from the complete callable type, curried layers and repeated products produce one concrete body with the substituted count, same-signature captures stay distinct by capture substitution, constructor values and structural selections reserve typed artifacts, conditional implementations and functional dependencies bind the checker-selected method, effect-polymorphic callbacks carry a concrete row, defaults evaluate through their call steps, cross-module calls discover the dependency module's function, and coroutine/reactive thunks bind demand-driven. Fixed-point tests confirm materialization adds no instance or dependency and rebuilding the worklist reproduces the graph.

The same-signature capture fixture surfaced a Stage 3.3 root rule gap: a nested closure with a concrete signature but a non-empty relevant-parameter set (it captures an enclosing generic value that never appears in its own signature) was seeded as an eager root without an enclosing environment and could not resolve its captured parameter. `seed_eager_templates` now also requires the relevant-parameter set to be empty, so such closures are discovered only from their construction site with the enclosing environment; `concrete_signature_closures_with_enclosing_parameters_stay_demand_driven` records the regression. The legacy backend cannot yet emit that construct (it reports an unspecialized type parameter during LLVM emission), so the CLI smoke program intentionally avoids it; Stage 5's migration removes that limitation.

### Step 3 — Test-only legacy-emission comparison

- Record the legacy backend's typed discoveries under `#[cfg(test)]`: the specialization queue already carries `(FunctionId, CheckedFunctionType, substitutions)`; add typed constructor-adapter and structural-method records at their creation points.
- Add a test-only helper that compiles a lowered module with the legacy backend and returns those records, then match every legacy source-function specialization to an interned instance by template and concrete substitutions (evidence is not part of the legacy record) and every legacy constructor/structural discovery to a typed artifact request.
- Run the comparison over representative direct, closure, trait, formatting, constructor, cross-module, coroutine, and reactive programs.

**Gate:** Every currently emitted legacy specialization and generated constructor/structural discovery has a matching new instance or artifact request; differences are explainable only as Stage 4 helper artifacts.

**Step 3 — complete.** `codegen.rs` records the legacy constructor-adapter and structural-method discoveries under `#[cfg(test)]` and exposes `legacy_emissions`, which compiles a `LoweredModule` and returns the specialization queue plus those records. The transition test `legacy_emissions_are_represented_in_the_graph` compares template identity and recorded substitutions for every queued specialization through `instance_for_legacy_specialization` (evidence excluded because the queue records only the concrete callable type), and matches constructor/structural discoveries to `ConstructorAdapterKey`/`StructuralMethodKey` artifact requests by symbol, structural kind, and canonical arguments. Representative programs cover direct, curried, result-only, closure, trait/default/conditional, structural, constructor, formatting, coroutine, and reactive emission.

### Step 4 — Gates and handoff

- Run `cargo fmt --all -- --check`, `cargo check --workspace`, `cargo test --workspace`, and `git diff --check`; exercise CLI LLVM, object, and run paths with this worktree's standard library.
- Update this plan, [STAGE_3_SPECIALIZATION_BREAKDOWN.md](STAGE_3_SPECIALIZATION_BREAKDOWN.md), and [TYPED_LOWERING_PLAN.md](TYPED_LOWERING_PLAN.md) with actual results and mark Stage 3 complete.

**Gate:** The worklist reaches a fixed point, every emitted instance is concrete and validated, no unused generic template becomes an instance, repeated runs are deterministic, and the existing backend regression suite still passes.

**Step 4 — complete.** `cargo fmt --all -- --check`, `cargo check --workspace`, `cargo test --workspace` (1176 tests: 234 compiler unit, 487 compiler integration, 224 CLI, 108 module, plus the smaller suites), and `git diff --check` pass. The CLI `--emit llvm`, `--emit object`, and `run` paths were exercised against the worktree standard library with a program exercising generic instantiation, string formatting, and constructor values; the LLVM module (657562 bytes), object file (83144 bytes), and program exit (42) were produced successfully.

**Completion gate:** The graph validator proves catalog/arena/name agreement, complete concrete substitutions, resolved evidence, dependency and recursive back-edge integrity, and template-only absence in every emitted body. The focused fixtures cover every Stage 3.5 scenario through the full lowering pipeline, the fixed-point tests show materialization and rebuilding are stable, and the legacy-emission transition test matches every currently emitted source specialization and generated constructor/structural discovery to the new graph. The full workspace gate and CLI LLVM/object/run paths pass.

## Boundaries to preserve

- Stage 3.5 audits the Stage 3.3/3.4 graph; it does not intern keys, materialize bodies, generate Stage 4 helper bodies, or switch LLVM to the lowered graph.
- The test-only legacy records live behind `#[cfg(test)]` and never affect production emission; the legacy backend stays authoritative until Stage 5.
- Constructor adapters and structural methods are recorded as typed artifact requests in Stage 3.5 and remain Stage 4's generated bodies.
