# Stage 4.2 Plan: Fixed-Point Artifact Closure Engine

**Status:** In progress. Stage 4.1 is complete through `9601bfb` (artifact key families, `LoweredArtifactPlan` placeholders, artifact-owned edge lists, `LoweredInstanceRequest::Artifact`, `LoweredArtifactRequestRoot::Artifact`, `FormattingWrite`). Stage 4.2 Steps 1–3 (shared `GraphRecorder` with install/take and resumable traversal; incremental pending-instance materialization; the closure engine with placeholder hooks, use/edge storage, and the `Scan` instance root) are complete; Steps 4–6 are next; Stages 4.3–4.6 plug their families into the API defined here.

## Goal and boundary

Turn the one-shot Stage 3 pipeline (`build_specialization_worklist` → `materialize_instance_bodies`) into a deterministic fixed point in which:

- materialized instance bodies and initializer bodies are **scanned** for sites that need generated artifacts;
- every artifact is **expanded** exactly once into its plan, and expansion may request further artifacts and new source-function instances;
- every newly requested instance is traversed by the Stage 3.3 worklist (which may reserve more instances and artifacts) and materialized by Stage 3.4, then scanned in turn;
- the loop stops when a round reserves nothing.

Stage 4.2 builds the engine, the request/use recording, the determinism rules, the validators that prove the fixed point, and the removal of unresolved compiler-helper requests. It does **not** implement any family's real scanner or expander: Stage 4.2 ships placeholder expanders that keep the Stage 4.1 plan unchanged and request nothing, plus test-only hooks that exercise every chain shape. Stages 4.3 (structural/constructor/formatting), 4.4 (drop glue, finalizers, clone), 4.5 (coroutines, reactive runners), and 4.6 (extern adapters, runtime requirements) register real scanners and expanders.

No backend change, no ABI change, no change to legacy emission. For every program that has no Stage 4 artifact requests, all Stage 3 results stay byte-identical: instance ordinals, artifact ordinals, names, edges, and bodies.

Line references are against `9601bfb` and will drift; function names are authoritative.

## Current pipeline facts this plan relies on

- `Lowerer::lower` (`lower.rs` ~12690) runs `build_specialization_worklist`, `materialize_instance_bodies`, `validate_instance_bodies`, `validate_specializations`, `validate_specialization_graph`, then `validate_source_coverage`, each only if the previous stage produced no diagnostics.
- `worklist::build` constructs a `WorklistBuilder` over `&LoweredProgram` with its **own** `SpecializationCatalog`, instance arena, artifact arena, and helper list, then returns `SpecializationParts`, which `build_specialization_worklist` installs on the program. The builder reads only template arenas and semantic catalogs from the program, never `program.instances`.
- `WorklistBuilder` has an internal `queue: Vec<FunctionInstanceId>` and `cursor`. `intern_resolved` appends an instance and queues it only when its ordinal is new. `request_artifact` appends an artifact only when its ordinal is new. Both record the edge on the owner. `TraversalOwner::Initializer` records **no** edge (only the request root).
- `assign_names` derives every name from `SpecializationCatalog::planned_names`, which is ordinal-based (`__staple_instance_{n}`, family-prefixed artifact names). Appending never renames earlier entries.
- `materialize_instance_bodies` builds `BodyMaterializer`, which snapshots `instances_by_key`/`artifacts_by_key` from the catalog and clones **every** instance's body. `BodyCloner` reads `program.instances` (`enclosing_request`) and diagnoses any site whose resolved key is not already interned, so the graph must be complete for an instance before that instance is materialized.
- `validate_instance_bodies` requires every binding to match a recorded dependency or artifact edge by target, kind, and origin, and vice versa.
- `LoweredCallableTarget::CompilerHelper` has no producer in source lowering (only tests construct it). The worklist turns it into a `LoweredCompilerHelperRequest` (or a diagnostic for an artifact owner) and materialization binds `LoweredBoundTarget::Helper`. In practice `helper_requests` is always empty.
- Initializer bodies are concrete template arenas with no materialized body and no binding table.

## Completion gate

- `Lowerer::lower` runs the closure loop after Stage 3 materialization and before the existing validators. The validators run once over the final, closed graph.
- The loop is deterministic, append-only, and terminates. Stage 3 ordinals, names, and edges are preserved as a prefix of the final catalog.
- Every artifact has been expanded exactly once and carries a plan whose key rebuilds (`matches_key`). Every newly requested instance has a materialized body (when its template has one) and has been scanned.
- Scanner-produced artifact uses are recorded on their owner (instance body or initializer) and agree one-to-one with owner artifact edges. Expander-produced requests are recorded as artifact-owned edges.
- A closure re-check proves the fixed point: re-scanning every owner and re-expanding every artifact against the final catalog reserves nothing new.
- `helper_requests` and `LoweredBoundTarget::Helper` are gone, and a `CompilerHelper` target that reaches the graph is a lowering diagnostic.
- Focused synthetic-hook tests (below) and the full workspace gate pass.

## Design

### Pipeline shape

```text
build_specialization_worklist     (Stage 3.3, unchanged entry point)
materialize_instance_bodies       (Stage 3.4, now materializes only pending instances)
close_artifact_catalog            (new, Stage 4.2)
  loop {
    scan unscanned initializers, then unscanned instances   → apply requests
    expand unexpanded artifacts, in ordinal order            → apply requests after each artifact
    if no new instance was reserved this round: break
    resume worklist over the new instances                   → may reserve more instances/artifacts
    materialize the new instances
  }
  re-assign names
validate_instance_bodies / validate_specializations / validate_specialization_graph / validate_artifact_closure (new) / validate_source_coverage
```

Recommendation: put the engine in a new private child module `lower/artifact_closure.rs`. Keep per-family scanners and expanders out of it. They live in later substage modules (or in `artifact_plan.rs`) and register through the hook types below.

### Shared graph recorder

Extract the interning and edge-recording half of `WorklistBuilder` (`catalog`, `instances`, `artifacts`, `intern_resolved`, `record_instance_edge`, `request_artifact`, `assign_names`, and the pending-instance queue) into a `GraphRecorder` owned by `lower/worklist.rs`. Both the worklist traversal and the closure loop use it, so there is exactly one place that reserves ordinals, creates records, and writes edges.

- `SpecializationParts` becomes the recorder's persisted state. Install it on the program with `LoweredProgram::install_graph(parts)` and take it back with `take_graph() -> SpecializationParts` (a `std::mem::take` of the four fields). The worklist traversal borrows `&LoweredProgram` for templates while the recorder is detached, exactly as `build` does today.
- Add `WorklistBuilder::resume(program, parts, pending: Vec<FunctionInstanceId>) -> Result<SpecializationParts, Vec<Diagnostic>>`. It seeds nothing and traverses only `pending` plus anything newly queued. `build` becomes `seed_initializers` + `seed_eager_templates` + the same drain loop. Traversal of an already-traversed instance must never recur: the recorder tracks a `traversed` watermark (the queue is append-only, so a cursor suffices).
- `assign_names` runs at the end of the closure (and still at the end of `build`, so Stage 3-only callers and tests keep working). Validation (`validate_specializations`) already checks name stability against `planned_names`.

### Collect-then-apply requests

Scanners and expanders take `&LoweredProgram` (read-only, graph installed) and return an ordered `Vec<ClosureRequest>`. The closure loop then detaches the graph and applies the requests with the recorder. This avoids aliasing the program mutably while reading instance bodies, and makes order explicit.

```rust
pub(super) enum ClosureRequest {
    /// A source-function instance, already resolved through
    /// `LoweredProgram::resolve_instance_request` by the requester.
    Instance {
        resolved: ResolvedInstanceRequest,
        kind: LoweredInstanceDependencyKind,
        origin: Origin,
    },
    /// A generated artifact with the plan it is created with (the family
    /// placeholder until its expander runs).
    Artifact {
        key: ArtifactRequestKey,
        plan: LoweredArtifactPlan,
        kind: LoweredArtifactDependencyKind,
        origin: Origin,
        /// The owner-local site that uses the artifact (scanner requests only).
        use_site: Option<ArtifactUseSite>,
    },
}
```

- The owner comes from the apply call, not from the request. That is `TraversalOwner::{Initializer, Instance, Artifact}`, which is already defined.
- Instance requests from expanders are resolved by the requester with `InstanceResolutionTarget::Root` and a fully concrete recipe (template signature substituted, explicit `CallSubstitutions` and `TraitEvidence`). Resolution failure is a diagnostic at the request origin. The engine never guesses substitutions. If a resolved instance is new, it is appended to the recorder's pending queue with `LoweredInstanceRequest::Artifact` or a new `LoweredInstanceRequest::Scan` root, depending on the owner.
- Decision for expander-requested instances: record the instance edge on the artifact (`LoweredArtifactRequest::instances`, as 4.1 defined). A scanner may also request an instance (for example buffer clone's element `Clone` in 4.4). That edge goes on the owner exactly like a Stage 3 dependency, with a new `LoweredInstanceDependencyKind` supplied by the scanner family. Stage 4.2 adds no concrete kinds itself. It makes the kind enum extensible, and its validators check scanner instance edges against uses the same way as artifact edges.

### Hook API for families

```rust
pub(super) trait ArtifactFamilyHooks {
    /// Sites in one initializer body that need artifacts, in lowered
    /// evaluation order.
    fn scan_initializer(&self, program: &LoweredProgram, initializer: InitializerId) -> ScanResult;
    /// Sites in one materialized instance body, in lowered evaluation order.
    fn scan_instance(&self, program: &LoweredProgram, instance: FunctionInstanceId) -> ScanResult;
    /// Expand one artifact. Returns the finished plan and the plan's own
    /// ordered requests. Called exactly once per artifact.
    fn expand(&self, program: &LoweredProgram, artifact: LoweredArtifactRequestId) -> ExpansionResult;
}
```

- `ScanResult` is `Result<Vec<ClosureRequest>, Vec<Diagnostic>>`. `ExpansionResult` is `Result<(LoweredArtifactPlan, Vec<ClosureRequest>), Vec<Diagnostic>>`.
- `ProductionHooks` dispatches `expand` by `ArtifactRequestKey` family with an exhaustive match, and composes per-family scanners in a fixed, documented order: 4.3 → 4.4 → 4.5 → 4.6. Each later substage replaces its family's placeholder arm. In Stage 4.2 every scanner returns nothing, and every expander returns the existing placeholder plan (`artifact.plan.clone()`) with no requests. Stage 4.1 guarantees that every artifact already has one.
- Tests supply a `#[cfg(test)]` hooks implementation (see Step 5). `close_artifact_catalog` takes `&dyn ArtifactFamilyHooks`, and `Lowerer::lower` passes `&ProductionHooks`.
- Within one owner, a scanner reports sites in lowered evaluation order and requests within a site in a documented per-family order. Across families, requests are concatenated in the fixed family order above. This order determines ordinals, so any change to it is a snapshot-visible change.

### Owner-side artifact uses

Scanner requests must be attributable to an exact site so Stage 5 can emit the reference and the validator can prove one-to-one agreement.

- Add `ArtifactUseSite`, an owner-local site enum. Stage 4.2 defines it with no family variants beyond a `#[cfg(test)] Test(u32)` variant. Stages 4.4 and 4.5 add variants such as drop facts, allocations, closure constructions, reactive operations, `coro` creations, and intrinsic calls. Every match on it must be exhaustive.
- Add `artifact_uses: Vec<LoweredArtifactUse { site: ArtifactUseSite, artifact: ArtifactOrdinal, kind, origin }>` to `LoweredInstanceBody`, in scan order.
- Initializers have no body record. Add `initializer_artifact_uses: Vec<Vec<LoweredArtifactUse>>` (indexed by `InitializerId`) plus `initializer_artifacts: Vec<Vec<LoweredArtifactDependency>>` on `LoweredProgram`, so initializer-owned artifact edges become explicit. (`TraversalOwner::Initializer` records no edge today.) Apply these only to closure-requested artifacts. Stage 3 initializer requests keep their existing request-root-only representation, so Stage 3 snapshots are unchanged.
- Note for Stage 5, and out of scope here: initializer dispatch sites still have no binding table at all. Record this in the handoff rather than fixing it.

### Round structure and determinism

- **Watermarks, not sets.** Initializers, instances, and artifacts are append-only. The engine keeps `scanned_initializers`, `scanned_instances`, and `expanded_artifacts` cursors. Every owner is scanned exactly once, and every artifact is expanded exactly once. A materialized body is immutable, so a single scan suffices.
- **Scan order within a round:** initializers in `InitializerId` order (first round only), then instances in `FunctionInstanceId` order from the scan cursor to the current end. Apply each owner's requests immediately after scanning it, so first-discovery order follows owner order.
- **Expansion order within a round:** artifacts in ordinal order from the expansion cursor. Expanding one artifact may append more, and the cursor keeps going until it reaches the end. Apply each artifact's requests immediately after its expansion, then write its finished plan.
- **End of round:** if the recorder's pending-instance queue is empty, the loop is finished. Otherwise, resume the worklist over the pending instances (which may append more instances and artifacts), materialize every instance that has a body-bearing template and no body yet, and start the next round. Newly materialized instances are scanned next round. Artifacts reserved by the resumed worklist (constructor/structural) are expanded next round.
- **Termination:** each key family is finite for a program that passes checking, because polymorphic recursion is rejected and type-keyed artifacts use compact nominal keys. The engine still enforces a defensive bound (recommendation: a round limit of 64, and a total growth limit proportional to template count × 64). When the bound is exceeded, it diagnoses at the origin of the last requested entry and walks `request` roots to print the requester chain. Choose limits that the full standard library and every fixture stay far below. Record the observed maxima in the step notes.
- **Stage 3 invariance:** with `ProductionHooks` in Stage 4.2, the loop performs one round, scans without requesting, and "expands" each existing artifact to its unchanged placeholder. Assert this in a test: the normalized snapshot equals the pre-4.2 snapshot for the existing fixtures.

### Incremental materialization

- Split `materialize_instance_bodies` into `materialize_pending_instance_bodies`, which materializes every instance that has no body and whose template has a body, in ordinal order. The existing entry point calls it once.
- `BodyMaterializer::new` rebuilds `instances_by_key`/`artifacts_by_key` from the current catalog on each call. Correctness requires that a pending instance's nested requests are all interned before materialization. The resumed worklist guarantees this. If it is violated, the existing "missing function instance" diagnostic fires. Treat that as a traversal bug, not something to paper over.
- Keep all-or-nothing installation per call: if any body fails, return diagnostics and install nothing.

### Compiler-helper removal

- Validate that `helper_requests` is empty after Stage 3.3 (it always is from source), then delete `LoweredCompilerHelperRequest`, `LoweredHelperRequester`, `helper_requests`, `SpecializationParts::helper_requests`, and `LoweredBoundTarget::Helper`.
- `WorklistBuilder::request_helper` and the two `BodyCloner` helper arms become diagnostics for every owner: "compiler-helper target `{function}` has no generated artifact". Keep `LoweredCallableTarget::CompilerHelper` and `LoweredCallableCategory::CompilerHelper` in the Stage 2 schema so the Stage 2 category decision-table tests keep their eight categories. Update the table's doc comment to say that the graph rejects the category.
- Rewrite `artifact_owned_helper_requests_are_diagnosed_not_panics` to cover every owner kind.

### Validation (`validate_artifact_closure`)

Validation runs after the closure loop in `Lowerer::lower`, next to the existing validators. Place it in `artifact_closure.rs` or extend `graph_validation.rs`, but it is a separate entry point.

- **Expansion completeness:** every artifact has `Some(plan)`, has been expanded (keep a per-artifact `expanded: bool` or compare against the stored cursor), and `plan.matches_key(key, origin)` holds. `validate_specializations` already checks the key match. Do not duplicate it.
- **Requester integrity:** every artifact's request root names an existing owner, and that owner records an edge to it with the same kind and origin. This includes the new initializer edge storage and artifact-owned roots. Every instance with an `Artifact` or `Scan` request root has a matching instance edge on that owner.
- **Use/edge agreement:** for each instance body and initializer, the multiset of `artifact_uses` equals the owner's closure-phase artifact edges by artifact, kind, and origin. Every use site is valid for that owner (the site's arena ID exists in that body). Stage 4.2 can only check the test variant. Later families add their site checks through an exhaustive match.
- **Materialization completeness:** every instance whose template has a body has a body. `validate_instance_bodies` already does this, so rely on it.
- **Fixed-point proof:** re-run every scanner and expander read-only against the final program. Map each returned request to its key (for instance requests, the resolved `InstanceKey`) and require that the key is already interned and the corresponding edge already exists. Any unknown key is a "closure did not reach a fixed point" diagnostic. This check also catches nondeterministic or order-sensitive hooks.
- **Acyclic request roots:** following `request` roots from any instance or artifact terminates at an initializer, an eager template, or a Stage 3 instance root. Recursion is fine in edges but not in roots.

## Implementation sequence

### Step 1 — Extract `GraphRecorder` and install/take helpers

- Move the interning, edge-recording, queue, and naming state from `WorklistBuilder` into `GraphRecorder`, with no behavior change. Add `LoweredProgram::{install_graph, take_graph}` and use them in `build_specialization_worklist`.
- Add `WorklistBuilder::resume` and the `traversed` watermark.
- **Gate:** all existing tests pass unchanged. A new test builds the graph once with `build`, and again with `build` restricted to initializers plus `resume` over the eager roots. The two normalized snapshots must be identical.

**Step 1 notes (complete).** `GraphRecorder` in `lower/worklist.rs` now owns the instance/artifact arenas, helper list, catalog, pending queue, and cursor; `WorklistBuilder` keeps only the program borrow, recorder, diagnostics, and traversal-visited sets, and delegates interning, edge recording, artifact requests, and naming. `LoweredProgram::{install_graph, take_graph}` move the recorder's persisted state (`SpecializationParts`) on and off the program. `WorklistBuilder::resume(program, parts, pending)` seeds nothing, enqueues `pending`, drains the queue, and re-assigns names. The traversal watermark is a transient `traversed: bool` on `LoweredFunctionInstance` rather than a recorder cursor, because the graph round-trips through `SpecializationParts` on every detach/install and the flag survives that round trip exactly like the rest of the record; `traverse_instance` returns early when it is set. `seed_eager_templates`/`request_eager` now report the roots they newly intern, which is what `resume` needs. New test `split_build_and_resume_match_the_full_build` seeds initializers, interns eager roots without draining, then resumes and requires an identical normalized graph snapshot and a clean `validate_specializations`. Existing worklist tests, `cargo fmt --all -- --check`, `cargo check --workspace`, and `cargo test --workspace` (1187 tests) pass.

### Step 2 — Incremental materialization

- Add `materialize_pending_instance_bodies` and route the existing entry point through it.
- **Gate:** existing Stage 3.4 tests and snapshots are unchanged. A test materializes, appends a new concrete instance through `GraphRecorder` plus `resume`, materializes again, and shows that the old bodies are untouched (pointer-equal or snapshot-equal) while the new body passes `validate_instance_bodies`.

**Step 2 notes (complete).** `LoweredProgram::materialize_instance_bodies` now delegates to `materialize_pending_instance_bodies`, and `BodyMaterializer::build_pending` clones only instances whose `body` is `None`. Behavior on the first call is unchanged, including the Stage 3.4 rule that a template with no runtime body still receives its empty body record (`bodyless_templates_materialize_empty_bodies`), so the plan's "whose template has a body" filter was deliberately not applied. Installation remains all-or-nothing per call. New test `resumed_instances_materialize_without_touching_installed_bodies` materializes a program, snapshots every installed body, appends a second concrete `identity` instance through `GraphRecorder` and `resume`, materializes again, and requires the old body snapshots to be byte-identical while the new instance's concrete body passes `validate_instance_bodies`. Full workspace suite (1188 tests) passes.

### Step 3 — Engine, requests, hooks, and use storage

- Add `lower/artifact_closure.rs` with `ClosureRequest`, `ArtifactFamilyHooks`, `ProductionHooks` (placeholder expanders, empty scanners), `ArtifactUseSite` (test variant only), `LoweredArtifactUse`, the initializer use/edge storage, the new instance request root for scanner-requested instances, cursors, the round loop, the termination bound, and the requester-chain diagnostic.
- Wire `close_artifact_catalog(&ProductionHooks)` into `Lowerer::lower` after `materialize_instance_bodies`.
- **Gate:** the Stage 3 invariance test passes (existing fixtures produce identical normalized snapshots with the closure enabled). The full workspace suite passes.

### Step 3 — Engine, requests, hooks, and use storage

**Step 3 notes (complete).** `lower/artifact_closure.rs` now holds the engine. `ClosureRequest` carries a pre-resolved instance request or an artifact request with its plan, kind, origin, and optional scanner use site; `ArtifactFamilyHooks` is the scanner/expander surface; `ProductionHooks` registers no scanners and keeps every family's Stage 4.1 placeholder plan through an exhaustive key-family match. `ArtifactUseSite` currently has only a `#[cfg(test)] Test(u32)` variant (the enum is uninhabited in production builds), and `LoweredArtifactUse` records `{site, artifact, kind, origin}`. Instance bodies gained `artifact_uses` (scan order); `LoweredProgram` gained `initializer_artifact_uses` and `initializer_artifacts`, both indexed by `InitializerId` and populated only for closure-phase initializer requests, so Stage 3 initializer requests keep their request-root-only representation. `LoweredInstanceRequest::Scan { owner }` records instances first discovered by a scanner on an instance or initializer body, and `LoweredInstanceRequest::origin` centralizes root-origin extraction. The loop scans initializers then materialized instances up to the end at phase start, expands artifacts in ordinal order through a cursor that absorbs artifacts appended during expansion, and when a round interned instances resumes the Stage 3.3 worklist over exactly those instances and materializes them. Requests are applied through `GraphRecorder` after `take_graph`, so ordinals and edges still have one writer. Bounds: 64 rounds and a growth budget of `max(templates * 64, 1024)`, diagnosed with the last request origin and a depth-bounded requester chain. `close_artifact_catalog(&ProductionHooks)` runs in `Lowerer::lower` after Stage 3 materialization. With placeholder hooks the closure does exactly one round with zero growth; the new `closure_with_production_hooks_preserves_stage_3_snapshots` test proves the normalized program snapshot is byte-identical to the pre-closure Stage 3 program for a fixture with generic instances, constructor/structural artifacts, and a coroutine. Full workspace suite (1189 tests) passes.

### Step 4 — Remove compiler-helper requests

- Apply the compiler-helper removal above.
- **Gate:** the helper tests are rewritten, and no `Helper` binding or `helper_requests` symbol remains (`grep` clean). Workspace tests pass.

### Step 5 — Closure validation and synthetic-hook tests

- Implement `validate_artifact_closure`, including the fixed-point re-check, and wire it into `Lowerer::lower`.
- Add a `#[cfg(test)]` `TestHooks` driven by a small table of scripted behaviors. Use it to prove each of the following with real lowered fixtures:
  - **Artifact → artifact chain:** a scanned site requests `DropGlue(A)`, whose expansion requests `DropGlue(B)`, whose expansion requests a finalizer. The edges, roots, and ordinals come out in the documented order.
  - **Artifact → instance → artifact:** an expansion requests a generic function instance at a concrete type (resolved with a `Root` target). The instance is traversed and materialized, and scanning it requests a further artifact in the next round.
  - **Recursion through artifacts:** `DropGlue(T)` requests itself (directly and through a second key), and the result dedups to one ordinal with a back-edge. An instance requested by an artifact whose body re-requests the same artifact converges.
  - **Dedup across owners:** two instances that request the same type-keyed artifact share one ordinal, with two edges and two uses.
  - **Per-owner separation:** the same template site in two instances produces two per-site keys (use `ReactionRunner` keys with a test site).
  - **Stage 3 prefix invariance:** adding closure requests never renumbers or renames any Stage 3 instance or artifact.
  - **Determinism:** repeated lowering gives byte-identical snapshots, including under differently seeded `HashMap` construction orders (reuse the Stage 3.1 technique).
  - **Non-convergence:** a hook that keeps inventing fresh keys (for example nested product types of increasing depth) hits the bound and reports the requester chain instead of hanging.
  - **Corruption:** diagnostics fire for a missing use, an extra use, a missing owner edge, an unexpanded artifact, a stale plan, an unmaterialized pending instance, a cyclic request root, and a fixed-point violation (a hook that returns a new key only on the re-check).
- Extend the normalized snapshot in `lower.rs` tests to render instance/initializer `artifact_uses` and initializer artifact edges.
- **Gate:** all focused tests pass. `cargo fmt --all -- --check`, `cargo check --workspace`, `cargo test --workspace --quiet`, and `git diff --check` pass. The CLI `--emit llvm`, `--emit object`, and `run` paths succeed with the worktree standard library.

### Step 6 — Handoff documentation

- Record the hook contract for Stages 4.3–4.6 in this file. It must cover: what a scanner may read, the required site order, how to add an `ArtifactUseSite` variant and its validator arm, how an expander builds a `Root` instance request, which kinds to use, and where to register in `ProductionHooks`. Record the observed round and growth maxima for the standard library.
- Update [STAGE_4_GENERATED_ARTIFACTS_BREAKDOWN.md](STAGE_4_GENERATED_ARTIFACTS_BREAKDOWN.md) and [TYPED_LOWERING_PLAN.md](TYPED_LOWERING_PLAN.md) with what passed and what remains, including the Stage 5 note that initializer dispatch sites have no binding table.

## Risks and decisions to watch

- **Worklist/materializer disagreement.** Stage 3.4 re-resolves sites and must find every key already interned. Any route the resumed worklist traverses differently from `BodyCloner` surfaces as a "missing function instance" diagnostic. Fix the traversal. Do not make the materializer intern keys.
- **Scanner reads of concrete facts.** Scanners must read instance bodies (concrete drop/pass-mode facts from Stage 3.4 Step 4), never templates. Initializer bodies are already concrete. Stage 4.4 relies on this.
- **Order sensitivity.** Hook order, site order, and apply-immediately-after-each-owner all affect ordinals. The fixed-point re-check and determinism tests catch accidental hash-order dependence, but not a deliberate reordering. Treat snapshot diffs as review-required.
- **Validator ordering.** The existing validators now see artifact-requested instances and artifact-owned roots. `validate_specialization_graph` and `validate_instance_bodies` must accept `LoweredInstanceRequest::Artifact` and the scan root everywhere they match on request kinds. Grep every match on `LoweredInstanceRequest`.
- **Performance.** Rebuilding `instances_by_key` per materialization call is O(n) per round. That is fine at the expected round count (≤ a handful). Revisit only if the standard library shows otherwise.

## Stage boundary

- Stage 4.2 delivers the closure engine, request/use recording, and validators, with placeholder family hooks. The catalog is closed only relative to the registered hooks. Stages 4.3–4.6 make it closed relative to the legacy backend, and Stage 4.7 proves it against legacy emission.
- No artifact plan gains real content in Stage 4.2. No legacy backend or ABI change. `TypedModule` is not consulted by the engine.
