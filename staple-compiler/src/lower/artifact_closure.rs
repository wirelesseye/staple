//! Stage 4.2: the fixed-point generated-artifact closure engine.
//!
//! Stage 3 builds the reachable source-function instance graph and materializes
//! concrete bodies once. Generated artifacts (constructor adapters, structural
//! methods, drop glue, finalizers, coroutine pairs, reactive runners, extern
//! adapters) can request further artifacts and source-function instances, so
//! the catalog is closed by a round-based loop:
//!
//! 1. scan every not-yet-scanned initializer, then every not-yet-scanned
//!    materialized instance, in catalog order, and apply each site's requests
//!    immediately;
//! 2. expand every not-yet-expanded artifact in ordinal order, applying each
//!    expansion's requests immediately and writing the finished plan;
//! 3. when the round reserved at least one new instance, resume the Stage 3.3
//!    worklist over exactly those instances and materialize their bodies; the
//!    next round scans them and expands artifacts the worklist reserved;
//! 4. stop when a round reserved nothing.
//!
//! Scanners and expanders read the graph read-only and return ordered requests;
//! the engine detaches the graph and applies requests through the shared
//! `GraphRecorder`, so exactly one place reserves ordinals and writes edges.
//! With `ProductionHooks` (Stage 4.2) every scanner is empty and every expander
//! returns the Stage 4.1 placeholder plan unchanged, so the loop performs one
//! round and the Stage 3 catalog is preserved byte-for-byte. Stages 4.3-4.6
//! replace the placeholder arms with real family scanners and expanders.

use staple_syntax::{Diagnostic, Span};

use super::worklist::{GraphRecorder, LoweredScanOwner, TraversalOwner, WorklistBuilder};
use super::{
    ArenaId, ExpressionId, FunctionInstanceId, InitializerId, ItemId, LoweredArtifactDependency,
    LoweredArtifactDependencyKind, LoweredArtifactPlan, LoweredArtifactRequestId,
    LoweredArtifactRequestRoot, LoweredCallId, LoweredCallableValueId, LoweredCoroId,
    LoweredInstanceBody, LoweredInstanceDependency, LoweredInstanceDependencyKind,
    LoweredInstanceRequest, LoweredProgram, LoweredReactiveCallbackId, LoweredReactiveOperationId,
    Origin, PatternId, PlannedCalleeRef, PlannedCalleeRefMut, ResolvedInstanceRequest, SymbolId,
};
use crate::specialization::{ArtifactOrdinal, ArtifactRequestKey};

/// Defensive round bound. Polymorphic recursion is rejected by checking and
/// type-keyed artifacts use compact nominal keys, so a well-formed program
/// converges in a handful of rounds; the bound exists so a broken hook
/// diagnoses instead of hanging.
const MAX_CLOSURE_ROUNDS: usize = 64;

/// Defensive total-growth allowance, proportional to the number of function
/// templates. Chosen far above the observed standard-library maxima; see the
/// Stage 4.2 step notes for the recorded values.
const GROWTH_PER_TEMPLATE: usize = 64;
const MIN_GROWTH_BUDGET: usize = 1_024;

/// How far a non-convergence diagnostic walks the requester chain before it
/// stops. Request roots always point at older entries, so the walk terminates;
/// the cap keeps a corrupt graph from producing an unbounded message.
const MAX_REQUESTER_CHAIN: usize = 32;

/// One scanner- or expander-produced request, applied by the engine after the
/// hook returned and the graph was detached.
#[derive(Debug, Clone)]
pub(super) enum ClosureRequest {
    /// A source-function instance, already resolved through
    /// `LoweredProgram::resolve_instance_request` by the requester with a
    /// fully concrete recipe. The engine never guesses substitutions.
    Instance {
        resolved: ResolvedInstanceRequest,
        kind: LoweredInstanceDependencyKind,
        origin: Origin,
        /// The owner-local site that calls or references the instance.
        /// Scanner requests carry one; expander requests do not.
        use_site: Option<ArtifactUseSite>,
    },
    /// A generated artifact with the plan it is created with.
    Artifact {
        key: ArtifactRequestKey,
        plan: LoweredArtifactPlan,
        kind: LoweredArtifactDependencyKind,
        origin: Origin,
        /// The owner-local site that uses the artifact. Scanner requests carry
        /// one; expander requests do not.
        use_site: Option<ArtifactUseSite>,
    },
}

/// Sites in one owner body that use a generated artifact. Stage 4.2 defines no
/// family variants; Stage 4.4 adds the ownership-cleanup sites (drops,
/// finalizers, and buffer clones); Stage 4.5 adds `coro` creations, reactive
/// callback/evaluator environments, and reactive runners; Stage 4.6 adds the
/// extern callable-value site. Every match on this enum must stay exhaustive,
/// and every variant's ID must be interpreted in the owning body's own arenas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ArtifactUseSite {
    /// A scripted test site identified by its position in the hook table.
    #[cfg(test)]
    Test(u32),
    /// A discarded expression-statement result (`drop_result`).
    DiscardedResult(ItemId),
    /// An assignment that drops the place's previous value (`drop_previous`).
    ReplacedValue(ItemId),
    /// A loop body's result dropped before the back edge (`drops_body_result`).
    LoopBodyResult(ExpressionId),
    /// A call temporary dropped after the call (`drops_after_call`).
    CallTemporary {
        call: LoweredCallId,
        argument: usize,
    },
    /// An extern call's C-string temporary drop.
    CStringTemporary(LoweredCallId),
    /// A wildcard pattern discarding a droppable value.
    WildcardDiscard(PatternId),
    /// An owned binding's scope-exit drop; also the owned-binding record. The
    /// symbol is a semantic catalog ID; the owning use record disambiguates
    /// which owner-local binding it names.
    OwnedBinding(SymbolId),
    /// A captured binding cell's finalizer.
    CellFinalizer(SymbolId),
    /// A closure environment's finalizer.
    ClosureEnvironment(LoweredCallableValueId),
    /// A managed `Ref` allocation's payload finalizer.
    RefConstruction(LoweredCallId),
    /// The `Drop` intrinsic's argument drop.
    DropIntrinsic(LoweredCallId),
    /// A C-string conversion dropping its source `CString`.
    CStringConversion(LoweredCallId),
    /// A completion intrinsic dropping an orphaned handle.
    CompletionOrphan(LoweredCallId),
    /// A buffer allocation's element finalizer.
    BufferAllocation(LoweredCallId),
    /// A buffer clone's destination-buffer finalizer.
    BufferCloneFinalizer(LoweredCallId),
    /// A buffer clone's per-element `Clone` call (an instance use).
    BufferCloneElement(LoweredCallId),
    /// A `coro` creation's resume/cleanup pair, at the `Coro` expression.
    CoroCreation(LoweredCoroId),
    /// A reaction/batch/`until` callback thunk's installed environment
    /// finalizer, before the runner's own use.
    ReactiveCallbackEnvironment(LoweredReactiveCallbackId),
    /// A derived evaluator thunk's installed environment finalizer, before the
    /// runner's own use.
    DerivedEvaluatorEnvironment(LoweredReactiveOperationId),
    /// A reaction, `until`, or derived runner, after the callback
    /// environment use.
    ReactiveRunner(LoweredReactiveOperationId),
    /// A non-variadic extern binding used as a first-class callable value.
    ExternAdapterValue(super::LoweredCallableValueId),
}

/// One artifact use recorded on its owner in scan order. The validator proves
/// the owner's uses agree one-to-one with its closure-phase artifact edges.
#[derive(Debug, Clone)]
pub(crate) struct LoweredArtifactUse {
    pub site: ArtifactUseSite,
    pub artifact: ArtifactOrdinal,
    pub kind: LoweredArtifactDependencyKind,
    pub origin: Origin,
}

/// One scanner-requested source-function instance use recorded on its owner
/// in scan order. The validator proves the owner's instance uses agree
/// one-to-one with its closure-phase instance edges, so every such edge is
/// tied to the exact site Stage 5 emits the reference from.
#[derive(Debug, Clone)]
pub(crate) struct LoweredInstanceUse {
    pub site: ArtifactUseSite,
    pub instance: FunctionInstanceId,
    pub kind: LoweredInstanceDependencyKind,
    pub origin: Origin,
}

pub(super) type ScanResult = Result<Vec<ClosureRequest>, Vec<Diagnostic>>;
pub(super) type ExpansionResult =
    Result<(LoweredArtifactPlan, Vec<ClosureRequest>), Vec<Diagnostic>>;

/// The family hook surface. Scanners read one concrete owner body and report
/// the artifacts it needs in lowered evaluation order; the expander reads one
/// artifact request and reports its finished plan and the plan's own ordered
/// requests. Called exactly once per owner/artifact.
pub(super) trait ArtifactFamilyHooks {
    /// Sites in one module initializer body that need artifacts, in lowered
    /// evaluation order.
    fn scan_initializer(&self, program: &LoweredProgram, initializer: InitializerId) -> ScanResult;
    /// Sites in one materialized instance body, in lowered evaluation order.
    fn scan_instance(&self, program: &LoweredProgram, instance: FunctionInstanceId) -> ScanResult;
    /// Expand one artifact. Returns the finished plan and the plan's own
    /// ordered requests. Called exactly once per artifact.
    fn expand(
        &self,
        program: &LoweredProgram,
        artifact: LoweredArtifactRequestId,
    ) -> ExpansionResult;

    /// Whether this hook set fills the owned body for `key`'s family.
    /// Validation only rejects a plan that still carries the request-time
    /// marker in a family whose expander is registered; a hook set or a
    /// family that keeps the Stage 4.1 placeholder plan is exempt.
    fn expands_body(&self, _key: &ArtifactRequestKey) -> bool {
        false
    }
}

/// The production hook set. Stages 4.3-4.6 register every family's scanner in
/// the fixed 4.3 -> 4.4 -> 4.5 -> 4.6 order and every family's expander, so
/// the closure loop closes the whole catalog.
pub(super) struct ProductionHooks;

impl ArtifactFamilyHooks for ProductionHooks {
    fn scan_initializer(&self, program: &LoweredProgram, initializer: InitializerId) -> ScanResult {
        // Family scanners are composed in the fixed order 4.3 -> 4.4 -> 4.5 ->
        // 4.6. Stage 4.4 owns the ownership-cleanup scanner; Stage 4.5 appends
        // the coroutine and reactive sites.
        let mut requests = super::cleanup_artifacts::scan_initializer(program, initializer)?;
        requests.extend(super::coroutine_artifacts::scan_initializer(
            program,
            initializer,
        )?);
        requests.extend(super::extern_artifacts::scan_initializer(
            program,
            initializer,
        )?);
        Ok(requests)
    }

    fn scan_instance(&self, program: &LoweredProgram, instance: FunctionInstanceId) -> ScanResult {
        let mut requests = super::cleanup_artifacts::scan_instance(program, instance)?;
        requests.extend(super::coroutine_artifacts::scan_instance(
            program, instance,
        )?);
        requests.extend(super::extern_artifacts::scan_instance(program, instance)?);
        Ok(requests)
    }
    fn expand(
        &self,
        program: &LoweredProgram,
        artifact: LoweredArtifactRequestId,
    ) -> ExpansionResult {
        let record = match program.artifacts.get(artifact) {
            Some(record) => record,
            None => {
                return Err(vec![Diagnostic::new(
                    Span::Compiler,
                    format!(
                        "artifact closure cannot expand missing artifact {}",
                        artifact.index()
                    ),
                )]);
            }
        };
        let key = match program.specializations.artifact(record.ordinal) {
            Some(key) => key,
            None => {
                return Err(vec![Diagnostic::new(
                    record.origin.span.clone(),
                    format!("artifact {} has no catalog key", artifact.index()),
                )]);
            }
        };
        let plan = match record.plan.clone() {
            Some(plan) => plan,
            None => {
                return Err(vec![Diagnostic::new(
                    record.origin.span.clone(),
                    format!("artifact {} has no plan to expand", artifact.index()),
                )]);
            }
        };
        // Stage 4.3 registers the constructor-adapter expander here; the
        // remaining families keep their placeholder plan until their own
        // substage replaces its arm. The match is exhaustive so a new key
        // family is never silently ignored.
        match key {
            ArtifactRequestKey::ConstructorAdapter(_) => {
                let LoweredArtifactPlan::ConstructorAdapter(plan) = plan else {
                    return Err(vec![Diagnostic::new(
                        record.origin.span.clone(),
                        "constructor-adapter artifact carries a mismatched plan".to_string(),
                    )]);
                };
                super::structural_artifacts::expand_constructor_adapter(program, artifact, plan)
            }
            ArtifactRequestKey::StructuralMethod(_) => {
                let LoweredArtifactPlan::StructuralMethod(plan) = plan else {
                    return Err(vec![Diagnostic::new(
                        record.origin.span.clone(),
                        "structural-method artifact carries a mismatched plan".to_string(),
                    )]);
                };
                super::structural_artifacts::expand_structural_method(program, artifact, plan)
            }
            ArtifactRequestKey::DropGlue(_) => {
                let LoweredArtifactPlan::DropGlue(plan) = plan else {
                    return Err(vec![Diagnostic::new(
                        record.origin.span.clone(),
                        "drop-glue artifact carries a mismatched plan".to_string(),
                    )]);
                };
                super::cleanup_artifacts::expand_drop_glue(program, artifact, plan)
            }
            ArtifactRequestKey::GcFinalizer(_) => {
                let LoweredArtifactPlan::GcFinalizer(plan) = plan else {
                    return Err(vec![Diagnostic::new(
                        record.origin.span.clone(),
                        "gc-finalizer artifact carries a mismatched plan".to_string(),
                    )]);
                };
                super::cleanup_artifacts::expand_gc_finalizer(program, artifact, plan)
            }
            ArtifactRequestKey::CoroutineCodes(_) => {
                let LoweredArtifactPlan::CoroutineCodes(plan) = plan else {
                    return Err(vec![Diagnostic::new(
                        record.origin.span.clone(),
                        "coroutine-codes artifact carries a mismatched plan".to_string(),
                    )]);
                };
                super::coroutine_artifacts::expand_coroutine_codes(program, artifact, plan)
            }
            ArtifactRequestKey::ReactionRunner(_) => {
                let LoweredArtifactPlan::ReactionRunner(plan) = plan else {
                    return Err(vec![Diagnostic::new(
                        record.origin.span.clone(),
                        "reaction-runner artifact carries a mismatched plan".to_string(),
                    )]);
                };
                super::coroutine_artifacts::expand_reactive_runner(
                    program,
                    artifact,
                    plan,
                    super::coroutine_artifacts::ReactiveRunnerFamily::Reaction,
                )
            }
            ArtifactRequestKey::UntilRunner(_) => {
                let LoweredArtifactPlan::UntilRunner(plan) = plan else {
                    return Err(vec![Diagnostic::new(
                        record.origin.span.clone(),
                        "until-runner artifact carries a mismatched plan".to_string(),
                    )]);
                };
                super::coroutine_artifacts::expand_reactive_runner(
                    program,
                    artifact,
                    plan,
                    super::coroutine_artifacts::ReactiveRunnerFamily::Until,
                )
            }
            ArtifactRequestKey::DerivedRunner(_) => {
                let LoweredArtifactPlan::DerivedRunner(plan) = plan else {
                    return Err(vec![Diagnostic::new(
                        record.origin.span.clone(),
                        "derived-runner artifact carries a mismatched plan".to_string(),
                    )]);
                };
                super::coroutine_artifacts::expand_reactive_runner(
                    program,
                    artifact,
                    plan,
                    super::coroutine_artifacts::ReactiveRunnerFamily::Derived,
                )
            }
            ArtifactRequestKey::ExternAdapter(_) => {
                let LoweredArtifactPlan::ExternAdapter(plan) = plan else {
                    return Err(vec![Diagnostic::new(
                        record.origin.span.clone(),
                        "extern-adapter artifact carries a mismatched plan".to_string(),
                    )]);
                };
                super::extern_artifacts::expand_extern_adapter(program, artifact, plan)
            }
        }
    }

    fn expands_body(&self, key: &ArtifactRequestKey) -> bool {
        matches!(
            key,
            ArtifactRequestKey::ConstructorAdapter(_)
                | ArtifactRequestKey::StructuralMethod(_)
                | ArtifactRequestKey::DropGlue(_)
                | ArtifactRequestKey::GcFinalizer(_)
                | ArtifactRequestKey::CoroutineCodes(_)
                | ArtifactRequestKey::ReactionRunner(_)
                | ArtifactRequestKey::UntilRunner(_)
                | ArtifactRequestKey::DerivedRunner(_)
                | ArtifactRequestKey::ExternAdapter(_)
        )
    }
}

/// The owner whose scan or expansion produced the requests being applied.
#[derive(Debug, Clone, Copy)]
enum AppliedOwner {
    Initializer(InitializerId),
    Instance(FunctionInstanceId),
    Artifact(LoweredArtifactRequestId),
}

impl AppliedOwner {
    fn describe(self) -> String {
        match self {
            AppliedOwner::Initializer(initializer) => {
                format!("initializer {}", initializer.index())
            }
            AppliedOwner::Instance(instance) => format!("instance {}", instance.index()),
            AppliedOwner::Artifact(artifact) => format!("artifact {}", artifact.index()),
        }
    }

    fn traversal_owner(self) -> TraversalOwner {
        match self {
            AppliedOwner::Initializer(initializer) => TraversalOwner::Initializer(initializer),
            AppliedOwner::Instance(instance) => TraversalOwner::Instance(instance),
            AppliedOwner::Artifact(artifact) => TraversalOwner::Artifact(artifact),
        }
    }
}

/// The total-growth allowance of one closure run, measured against the Stage 3
/// catalog it started from.
struct GrowthBudget {
    baseline_instances: usize,
    baseline_artifacts: usize,
    limit: usize,
}

impl GrowthBudget {
    /// Reports non-convergence once the closure has appended more instances and
    /// artifacts than the budget allows. Checked after every apply, so neither
    /// an instance-growing nor an artifact-only chain can run unbounded.
    fn check(
        &self,
        program: &LoweredProgram,
        last_request: &Option<LastRequest>,
    ) -> Option<Diagnostic> {
        let growth = program
            .instances
            .len()
            .saturating_sub(self.baseline_instances)
            + program
                .artifacts
                .len()
                .saturating_sub(self.baseline_artifacts);
        (growth > self.limit).then(|| {
            program.non_convergence(
                last_request,
                format!(
                    "artifact closure grew by {growth} entries (budget {})",
                    self.limit
                ),
            )
        })
    }
}

/// The last request the engine applied, used to diagnose bound violations at
/// the requesting site.
struct LastRequest {
    origin: Origin,
    description: String,
}

impl LoweredProgram {
    /// Closes the artifact catalog relative to `hooks`: scans every owner once,
    /// expands every artifact once, resumes the worklist over instances first
    /// requested by artifacts, and materializes them, until a round reserves
    /// nothing. Returns every diagnostic without installing a partial closure
    /// result; the caller discards the program on failure.
    pub(super) fn close_artifact_catalog(
        &mut self,
        hooks: &dyn ArtifactFamilyHooks,
    ) -> Vec<Diagnostic> {
        // Initializer artifact storage is indexed by `InitializerId`. Stage 3
        // initializer requests stay request-root-only; closure-phase scans make
        // their uses and edges explicit.
        self.initializer_artifact_uses = vec![Vec::new(); self.initializers.len()];
        self.initializer_artifacts = vec![Vec::new(); self.initializers.len()];
        self.initializer_instance_uses = vec![Vec::new(); self.initializers.len()];
        self.initializer_instances = vec![Vec::new(); self.initializers.len()];
        self.initializer_owned_bindings = vec![Vec::new(); self.initializers.len()];
        self.initializer_bindings =
            vec![std::collections::BTreeMap::new(); self.initializers.len()];
        self.initializer_evidence =
            vec![std::collections::BTreeMap::new(); self.initializers.len()];

        let budget = GrowthBudget {
            baseline_instances: self.instances.len(),
            baseline_artifacts: self.artifacts.len(),
            limit: self
                .functions
                .iter()
                .count()
                .saturating_mul(GROWTH_PER_TEMPLATE)
                .max(MIN_GROWTH_BUDGET),
        };

        let mut scanned_initializers = 0usize;
        let mut scanned_instances = 0usize;
        let mut expanded_artifacts = 0usize;
        let mut last_request: Option<LastRequest> = None;

        for _round in 0..MAX_CLOSURE_ROUNDS {
            let mut new_instances = Vec::new();

            // Scan owners: initializers first (first round only, they never
            // grow), then materialized instances up to the end at phase start.
            while scanned_initializers < self.initializers.len() {
                let initializer = InitializerId::from_index(scanned_initializers);
                scanned_initializers += 1;
                let requests = match hooks.scan_initializer(self, initializer) {
                    Ok(requests) => requests,
                    Err(diagnostics) => return diagnostics,
                };
                let diagnostics = self.apply_closure_requests(
                    AppliedOwner::Initializer(initializer),
                    requests,
                    &mut new_instances,
                    &mut last_request,
                );
                if !diagnostics.is_empty() {
                    return diagnostics;
                }
                if let Some(diagnostic) = budget.check(self, &last_request) {
                    return vec![diagnostic];
                }
            }

            let scan_end = self.instances.len();
            while scanned_instances < scan_end {
                let instance = FunctionInstanceId::from_index(scanned_instances);
                scanned_instances += 1;
                let requests = match hooks.scan_instance(self, instance) {
                    Ok(requests) => requests,
                    Err(diagnostics) => return diagnostics,
                };
                let diagnostics = self.apply_closure_requests(
                    AppliedOwner::Instance(instance),
                    requests,
                    &mut new_instances,
                    &mut last_request,
                );
                if !diagnostics.is_empty() {
                    return diagnostics;
                }
                if let Some(diagnostic) = budget.check(self, &last_request) {
                    return vec![diagnostic];
                }
            }

            // Expand artifacts to the current end. Expanding one artifact may
            // append more, and the cursor keeps going until it reaches the
            // end, so every artifact is expanded exactly once per closure.
            // An expansion chain that only ever appends artifacts never ends
            // a round, so the growth budget is checked after every expansion
            // rather than only at the round boundary.
            while expanded_artifacts < self.artifacts.len() {
                let artifact = LoweredArtifactRequestId::from_index(expanded_artifacts);
                expanded_artifacts += 1;
                let (plan, requests) = match hooks.expand(self, artifact) {
                    Ok(result) => result,
                    Err(diagnostics) => return diagnostics,
                };
                let diagnostics = self.apply_closure_requests(
                    AppliedOwner::Artifact(artifact),
                    requests,
                    &mut new_instances,
                    &mut last_request,
                );
                if !diagnostics.is_empty() {
                    return diagnostics;
                }
                if let Some(record) = self.artifacts.get_mut(artifact) {
                    record.plan = Some(plan);
                    record.expanded = true;
                }
                if let Some(diagnostic) = budget.check(self, &last_request) {
                    return vec![diagnostic];
                }
            }

            if new_instances.is_empty() {
                let diagnostics = self.bind_artifact_plan_callees();
                if !diagnostics.is_empty() {
                    return diagnostics;
                }
                let diagnostics = super::cleanup_artifacts::collect_owned_bindings(self);
                if !diagnostics.is_empty() {
                    return diagnostics;
                }
                let diagnostics = self.record_runtime_requirements();
                if !diagnostics.is_empty() {
                    return diagnostics;
                }
                // Stage 5.1 (D4): the initializer binding tables need the same
                // closure fixed point, so they are built here, once, before
                // names are assigned.
                let diagnostics = self.bind_initializer_sites();
                if !diagnostics.is_empty() {
                    return diagnostics;
                }
                #[cfg(test)]
                {
                    self.closure_stats = Some(super::ClosureStats {
                        rounds: _round + 1,
                        growth: self
                            .instances
                            .len()
                            .saturating_sub(budget.baseline_instances)
                            + self
                                .artifacts
                                .len()
                                .saturating_sub(budget.baseline_artifacts),
                    });
                }
                return self.finish_closure();
            }

            // The next round needs bodies: resume the Stage 3.3 traversal over
            // exactly the newly interned instances, then materialize them.
            let parts = self.take_graph();
            let parts = match WorklistBuilder::resume(self, parts, new_instances) {
                Ok(parts) => parts,
                Err(diagnostics) => return diagnostics,
            };
            self.install_graph(parts);
            let diagnostics = self.materialize_pending_instance_bodies();
            if !diagnostics.is_empty() {
                return diagnostics;
            }
        }

        vec![self.non_convergence(
            &last_request,
            format!("artifact closure did not converge after {MAX_CLOSURE_ROUNDS} rounds"),
        )]
    }

    /// Resolves every planned callee key in every artifact plan to its final
    /// catalog id. Runs once the closure loop reaches a fixed point, after all
    /// instances and artifacts are interned and before names are assigned.
    ///
    /// A key with no catalog entry is a bug in the expander: expansion always
    /// emits the `ClosureRequest` that interns the key it names, so the
    /// diagnostic points at the plan rather than at a malformed request.
    fn bind_artifact_plan_callees(&mut self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for (_, artifact) in self.artifacts.iter_mut() {
            let Some(plan) = artifact.plan.as_mut() else {
                continue;
            };
            for callee in plan.planned_callees_mut() {
                match callee {
                    PlannedCalleeRefMut::Instance(planned) => {
                        match self.specializations.instance_ordinal(&planned.key) {
                            Some(ordinal) => {
                                planned.instance =
                                    Some(FunctionInstanceId::from_index(ordinal.index()));
                            }
                            None => diagnostics.push(Diagnostic::new(
                                artifact.origin.span.clone(),
                                format!(
                                    "generated artifact {} names a planned instance callee that was never interned",
                                    artifact.ordinal.index()
                                ),
                            )),
                        }
                    }
                    PlannedCalleeRefMut::Artifact(planned) => {
                        match self.specializations.artifact_ordinal(&planned.key) {
                            Some(ordinal) => planned.artifact = Some(ordinal),
                            None => diagnostics.push(Diagnostic::new(
                                artifact.origin.span.clone(),
                                format!(
                                    "generated artifact {} names a planned artifact callee that was never interned",
                                    artifact.ordinal.index()
                                ),
                            )),
                        }
                    }
                }
            }
        }
        diagnostics
    }

    /// Re-assigns catalog names over the final graph and returns any name
    /// collision diagnostics. Names are stable for a fixed catalog, so running
    /// this at the end of the closure (and again at the end of every resume)
    /// never renames an earlier entry.
    fn finish_closure(&mut self) -> Vec<Diagnostic> {
        let mut recorder = GraphRecorder::from_parts(self.take_graph());
        let result = recorder.assign_names(self.declared_name_resolver());
        self.install_graph(recorder.into_parts());
        result.err().unwrap_or_default()
    }

    /// Applies one owner's requests through the shared recorder, detaching the
    /// graph for the duration. Newly interned instances are collected for the
    /// round-end resume. Scanner artifact requests also record their use site.
    fn apply_closure_requests(
        &mut self,
        owner: AppliedOwner,
        requests: Vec<ClosureRequest>,
        new_instances: &mut Vec<FunctionInstanceId>,
        last_request: &mut Option<LastRequest>,
    ) -> Vec<Diagnostic> {
        if requests.is_empty() {
            return Vec::new();
        }
        let mut recorder = GraphRecorder::from_parts(self.take_graph());
        let mut diagnostics = Vec::new();
        for request in requests {
            match request {
                ClosureRequest::Instance {
                    resolved,
                    kind,
                    origin,
                    use_site,
                } => {
                    if matches!(owner, AppliedOwner::Artifact(_)) && use_site.is_some() {
                        diagnostics.push(Diagnostic::new(
                            origin.span.clone(),
                            "expander-produced instance requests carry no use site".to_string(),
                        ));
                        continue;
                    }
                    let request_root = match owner {
                        AppliedOwner::Initializer(initializer) => LoweredInstanceRequest::Scan {
                            owner: LoweredScanOwner::Initializer(initializer),
                            kind,
                            origin: origin.clone(),
                        },
                        AppliedOwner::Instance(instance) => LoweredInstanceRequest::Scan {
                            owner: LoweredScanOwner::Instance(instance),
                            kind,
                            origin: origin.clone(),
                        },
                        AppliedOwner::Artifact(artifact) => LoweredInstanceRequest::Artifact {
                            artifact,
                            kind,
                            origin: origin.clone(),
                        },
                    };
                    let owner_label = owner.describe();
                    let (instance, created) =
                        recorder.intern_resolved(self, resolved, request_root);
                    if created {
                        new_instances.push(instance);
                    }
                    let edge = LoweredInstanceDependency {
                        instance,
                        origin: origin.clone(),
                        kind,
                        closure_phase: true,
                    };
                    match owner {
                        AppliedOwner::Initializer(initializer) => {
                            // Closure-phase initializer instance edges become
                            // explicit; Stage 3 initializer instance requests
                            // keep their request-root-only representation.
                            self.initializer_instances[initializer.index()].push(edge);
                        }
                        AppliedOwner::Instance(_) | AppliedOwner::Artifact(_) => {
                            recorder.record_closure_instance_edge(
                                owner.traversal_owner(),
                                instance,
                                &origin,
                                kind,
                            );
                        }
                    }
                    if let Some(site) = use_site {
                        let use_ = LoweredInstanceUse {
                            site,
                            instance,
                            kind,
                            origin: origin.clone(),
                        };
                        match owner {
                            AppliedOwner::Initializer(initializer) => {
                                self.initializer_instance_uses[initializer.index()].push(use_);
                            }
                            AppliedOwner::Instance(owner) => {
                                if let Some(record) = recorder.instances.get_mut(owner)
                                    && let Some(body) = record.body.as_mut()
                                {
                                    body.instance_uses.push(use_);
                                }
                            }
                            AppliedOwner::Artifact(_) => {}
                        }
                    }
                    *last_request = Some(LastRequest {
                        origin,
                        description: format!(
                            "{owner_label} requested instance {} ({})",
                            instance.index(),
                            kind.description()
                        ),
                    });
                }
                ClosureRequest::Artifact {
                    key,
                    plan,
                    kind,
                    origin,
                    use_site,
                } => {
                    if matches!(owner, AppliedOwner::Artifact(_)) && use_site.is_some() {
                        diagnostics.push(Diagnostic::new(
                            origin.span.clone(),
                            "expander-produced artifact requests carry no use site".to_string(),
                        ));
                        continue;
                    }
                    let owner_label = owner.describe();
                    let (ordinal, _) = recorder.request_closure_artifact(
                        key,
                        Some(plan),
                        &origin,
                        owner.traversal_owner(),
                        kind,
                    );
                    if let Some(site) = use_site {
                        let use_ = LoweredArtifactUse {
                            site,
                            artifact: ordinal,
                            kind,
                            origin: origin.clone(),
                        };
                        match owner {
                            AppliedOwner::Initializer(initializer) => {
                                self.initializer_artifact_uses[initializer.index()].push(use_);
                            }
                            AppliedOwner::Instance(instance) => {
                                if let Some(record) = recorder.instances.get_mut(instance)
                                    && let Some(body) = record.body.as_mut()
                                {
                                    body.artifact_uses.push(use_);
                                }
                            }
                            AppliedOwner::Artifact(_) => {}
                        }
                    }
                    if let AppliedOwner::Initializer(initializer) = owner {
                        // Initializer-owned artifact edges become explicit for
                        // closure-requested artifacts. Stage 3 initializer
                        // requests keep their request-root-only representation.
                        self.initializer_artifacts[initializer.index()].push(
                            LoweredArtifactDependency {
                                artifact: ordinal,
                                origin: origin.clone(),
                                kind,
                                closure_phase: true,
                            },
                        );
                    }
                    *last_request = Some(LastRequest {
                        origin: origin.clone(),
                        description: format!(
                            "{owner_label} requested artifact {} ({})",
                            ordinal.index(),
                            kind.description()
                        ),
                    });
                }
            }
        }
        self.install_graph(recorder.into_parts());
        diagnostics
    }

    /// Builds the non-convergence diagnostic at the last request's origin,
    /// followed by the requester chain that reached it.
    fn non_convergence(&self, last_request: &Option<LastRequest>, headline: String) -> Diagnostic {
        let (span, detail) = match last_request {
            Some(last) => (last.origin.span.clone(), last.description.clone()),
            None => (Span::Compiler, "no request was recorded".to_string()),
        };
        let mut chain = vec![detail];
        if let Some(last) = last_request {
            self.request_chain_for_origin(&last.origin, &mut chain);
        }
        Diagnostic::new(
            span,
            format!("{headline}: request chain: {}", chain.join(" <- ")),
        )
    }

    /// Walks request roots from the entries that mention `origin`, appending
    /// the requester chain. The walk is depth-bounded defensively.
    fn request_chain_for_origin(&self, origin: &Origin, chain: &mut Vec<String>) {
        enum ChainNode {
            Instance(FunctionInstanceId),
            Artifact(LoweredArtifactRequestId),
        }

        let start = self
            .instances
            .iter()
            .find(|(_, record)| record.request.origin(&record.origin) == *origin)
            .map(|(id, _)| ChainNode::Instance(id))
            .or_else(|| {
                self.artifacts
                    .iter()
                    .find(|(_, record)| record.origin == *origin)
                    .map(|(id, _)| ChainNode::Artifact(id))
            });
        let Some(mut node) = start else {
            return;
        };
        for _ in 0..MAX_REQUESTER_CHAIN {
            match node {
                ChainNode::Artifact(id) => {
                    let Some(record) = self.artifacts.get(id) else {
                        return;
                    };
                    match &record.request {
                        LoweredArtifactRequestRoot::Initializer { initializer, .. } => {
                            chain.push(format!("initializer {}", initializer.index()));
                            return;
                        }
                        LoweredArtifactRequestRoot::Instance {
                            instance: owner,
                            kind,
                            ..
                        } => {
                            chain.push(format!(
                                "artifact {} <- instance {} ({})",
                                id.index(),
                                owner.index(),
                                kind.description()
                            ));
                            node = ChainNode::Instance(*owner);
                        }
                        LoweredArtifactRequestRoot::Artifact {
                            artifact: owner,
                            kind,
                            ..
                        } => {
                            chain.push(format!(
                                "artifact {} <- artifact {} ({})",
                                id.index(),
                                owner.index(),
                                kind.description()
                            ));
                            node = ChainNode::Artifact(*owner);
                        }
                    }
                }
                ChainNode::Instance(id) => {
                    let Some(record) = self.instances.get(id) else {
                        return;
                    };
                    match &record.request {
                        LoweredInstanceRequest::Initializer {
                            initializer, kind, ..
                        } => {
                            chain.push(format!(
                                "instance {} <- initializer {} ({})",
                                id.index(),
                                initializer.index(),
                                kind.description()
                            ));
                            return;
                        }
                        LoweredInstanceRequest::EagerTemplate => {
                            chain.push(format!("instance {} <- eager template", id.index()));
                            return;
                        }
                        LoweredInstanceRequest::Dependency { owner, kind, .. } => {
                            chain.push(format!(
                                "instance {} <- instance {} ({})",
                                id.index(),
                                owner.index(),
                                kind.description()
                            ));
                            node = ChainNode::Instance(*owner);
                        }
                        LoweredInstanceRequest::Artifact {
                            artifact: owner,
                            kind,
                            ..
                        } => {
                            chain.push(format!(
                                "instance {} <- artifact {} ({})",
                                id.index(),
                                owner.index(),
                                kind.description()
                            ));
                            node = ChainNode::Artifact(*owner);
                        }
                        LoweredInstanceRequest::Scan { owner, kind, .. } => match owner {
                            LoweredScanOwner::Initializer(initializer) => {
                                chain.push(format!(
                                    "instance {} <- scan of initializer {} ({})",
                                    id.index(),
                                    initializer.index(),
                                    kind.description()
                                ));
                                return;
                            }
                            LoweredScanOwner::Instance(owner) => {
                                chain.push(format!(
                                    "instance {} <- scan of instance {} ({})",
                                    id.index(),
                                    owner.index(),
                                    kind.description()
                                ));
                                node = ChainNode::Instance(*owner);
                            }
                        },
                    }
                }
            }
        }
        chain.push("...".to_string());
    }
}

/// Where a fixed-point re-check request came from.
#[derive(Debug, Clone, Copy)]
enum FixedPointOwner {
    Initializer(InitializerId),
    Instance(FunctionInstanceId),
    Artifact(LoweredArtifactRequestId),
}

impl FixedPointOwner {
    fn describe(self) -> String {
        match self {
            FixedPointOwner::Initializer(initializer) => {
                format!("initializer {}", initializer.index())
            }
            FixedPointOwner::Instance(instance) => format!("instance {}", instance.index()),
            FixedPointOwner::Artifact(artifact) => format!("artifact {}", artifact.index()),
        }
    }
}

/// The comparable identity of one closure use or edge: target, kind, and
/// origin. Artifact and instance records are compared only within their own
/// family, so the kind description is unambiguous.
#[derive(Debug, Clone, Copy, PartialEq)]
struct UseEdge<'a> {
    target: usize,
    kind: &'static str,
    origin: &'a Origin,
}

impl<'a> UseEdge<'a> {
    fn artifact_edge(edge: &'a LoweredArtifactDependency) -> Self {
        UseEdge {
            target: edge.artifact.index(),
            kind: edge.kind.description(),
            origin: &edge.origin,
        }
    }

    fn artifact_use(use_: &'a LoweredArtifactUse) -> Self {
        UseEdge {
            target: use_.artifact.index(),
            kind: use_.kind.description(),
            origin: &use_.origin,
        }
    }

    fn instance_edge(edge: &'a LoweredInstanceDependency) -> Self {
        UseEdge {
            target: edge.instance.index(),
            kind: edge.kind.description(),
            origin: &edge.origin,
        }
    }

    fn instance_use(use_: &'a LoweredInstanceUse) -> Self {
        UseEdge {
            target: use_.instance.index(),
            kind: use_.kind.description(),
            origin: &use_.origin,
        }
    }
}

/// The owner whose arenas a use site's IDs must resolve in. Instance bodies
/// own private arenas; initializer sites index the program's template arenas.
#[derive(Clone, Copy)]
enum UseSiteOwner<'a> {
    Instance(&'a LoweredInstanceBody),
    Initializer(InitializerId),
}

/// The request-root walk node used by the acyclicity check.
#[derive(Debug, Clone, Copy)]
enum RequestRoot {
    Instance(FunctionInstanceId),
    Artifact(LoweredArtifactRequestId),
}

impl LoweredProgram {
    /// Validates the closed artifact catalog: expansion completeness, requester
    /// integrity, scanner use/edge agreement, acyclic request roots, and the
    /// fixed point itself. Runs after `close_artifact_catalog` and after the
    /// Stage 3 validators.
    pub(super) fn validate_artifact_closure(
        &self,
        hooks: &dyn ArtifactFamilyHooks,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        self.check_artifact_expansion(&mut diagnostics);
        self.check_requester_integrity(&mut diagnostics);
        self.check_use_edge_agreement(&mut diagnostics);
        self.check_planned_callees(hooks, &mut diagnostics);
        super::cleanup_artifacts::check_owned_bindings(self, &mut diagnostics);
        super::coroutine_artifacts::check_stage_4_5(self, &mut diagnostics);
        super::extern_artifacts::check_stage_4_6(self, &mut diagnostics);
        super::runtime_requirements::check_runtime_requirements(self, &mut diagnostics);
        self.check_request_root_acyclicity(&mut diagnostics);
        self.check_closure_fixed_point(hooks, &mut diagnostics);
        diagnostics
    }

    /// Every artifact must have been expanded exactly once by the closure loop.
    /// Plan presence and key agreement are already checked by
    /// `validate_specializations`; this check only proves expansion ran.
    fn check_artifact_expansion(&self, diagnostics: &mut Vec<Diagnostic>) {
        for (id, artifact) in self.artifacts.iter() {
            if !artifact.expanded {
                diagnostics.push(Diagnostic::new(
                    artifact.origin.span.clone(),
                    format!("generated artifact {} was never expanded", id.index()),
                ));
            }
        }
    }

    /// Every request root must name an existing owner, and that owner must
    /// record the matching edge with the same kind and origin. Initializer
    /// artifact edges are checked by use/edge agreement instead, because Stage
    /// 3 initializer requests keep their request-root-only representation.
    fn check_requester_integrity(&self, diagnostics: &mut Vec<Diagnostic>) {
        for (id, instance) in self.instances.iter() {
            match &instance.request {
                LoweredInstanceRequest::Initializer { initializer, .. } => {
                    if !self.initializers.contains(*initializer) {
                        diagnostics.push(Diagnostic::new(
                            instance.origin.span.clone(),
                            format!(
                                "function instance {} was requested by missing initializer {}",
                                id.index(),
                                initializer.index()
                            ),
                        ));
                    }
                }
                LoweredInstanceRequest::Artifact {
                    artifact,
                    kind,
                    origin,
                } => {
                    let recorded = self.artifacts.get(*artifact).is_some_and(|owner| {
                        owner.instances.iter().any(|edge| {
                            edge.instance == id && edge.kind == *kind && edge.origin == *origin
                        })
                    });
                    if !recorded {
                        diagnostics.push(Diagnostic::new(
                            origin.span.clone(),
                            format!(
                                "function instance {} is not recorded on the artifact that requested it",
                                id.index()
                            ),
                        ));
                    }
                }
                LoweredInstanceRequest::Scan {
                    owner,
                    kind,
                    origin,
                } => match owner {
                    LoweredScanOwner::Initializer(initializer) => {
                        if !self.initializers.contains(*initializer) {
                            diagnostics.push(Diagnostic::new(
                                origin.span.clone(),
                                format!(
                                    "function instance {} was requested by missing initializer {}",
                                    id.index(),
                                    initializer.index()
                                ),
                            ));
                            continue;
                        }
                        let recorded = self
                            .initializer_instances
                            .get(initializer.index())
                            .is_some_and(|edges| {
                                edges.iter().any(|edge| {
                                    edge.instance == id
                                        && edge.kind == *kind
                                        && edge.origin == *origin
                                })
                            });
                        if !recorded {
                            diagnostics.push(Diagnostic::new(
                                origin.span.clone(),
                                format!(
                                    "function instance {} is not recorded on the initializer scan that requested it",
                                    id.index()
                                ),
                            ));
                        }
                    }
                    LoweredScanOwner::Instance(owner) => {
                        let recorded = self.instances.get(*owner).is_some_and(|record| {
                            record.dependencies.iter().any(|edge| {
                                edge.instance == id
                                    && edge.kind == *kind
                                    && edge.origin == *origin
                                    && edge.closure_phase
                            })
                        });
                        if !recorded {
                            diagnostics.push(Diagnostic::new(
                                origin.span.clone(),
                                format!(
                                    "function instance {} is not recorded on the instance scan that requested it",
                                    id.index()
                                ),
                            ));
                        }
                    }
                },
                LoweredInstanceRequest::Dependency { .. }
                | LoweredInstanceRequest::EagerTemplate => {}
            }
        }

        for (id, artifact) in self.artifacts.iter() {
            match &artifact.request {
                LoweredArtifactRequestRoot::Initializer { initializer, .. } => {
                    if !self.initializers.contains(*initializer) {
                        diagnostics.push(Diagnostic::new(
                            artifact.origin.span.clone(),
                            format!(
                                "generated artifact {} was requested by missing initializer {}",
                                id.index(),
                                initializer.index()
                            ),
                        ));
                    }
                }
                LoweredArtifactRequestRoot::Instance {
                    instance,
                    kind,
                    origin,
                } => {
                    let recorded = self.instances.get(*instance).is_some_and(|record| {
                        record.artifacts.iter().any(|edge| {
                            edge.artifact == artifact.ordinal
                                && edge.kind == *kind
                                && edge.origin == *origin
                        })
                    });
                    if !recorded {
                        diagnostics.push(Diagnostic::new(
                            origin.span.clone(),
                            format!(
                                "generated artifact {} has no recorded edge on the instance that requested it",
                                id.index()
                            ),
                        ));
                    }
                }
                LoweredArtifactRequestRoot::Artifact {
                    artifact: owner,
                    kind,
                    origin,
                } => {
                    let recorded = self.artifacts.get(*owner).is_some_and(|record| {
                        record.artifacts.iter().any(|edge| {
                            edge.artifact == artifact.ordinal
                                && edge.kind == *kind
                                && edge.origin == *origin
                        })
                    });
                    if !recorded {
                        diagnostics.push(Diagnostic::new(
                            origin.span.clone(),
                            format!(
                                "generated artifact {} has no recorded edge on the artifact that requested it",
                                id.index()
                            ),
                        ));
                    }
                }
            }
        }
    }

    /// Every artifact plan must be fully expanded for a family whose expander
    /// is registered, and every callee a plan names must be bound to its
    /// catalog id and matched one-to-one, in plan order, by an artifact-owned
    /// edge of the same target and kind. No artifact-owned edge may lack a
    /// planned callee.
    fn check_planned_callees(
        &self,
        hooks: &dyn ArtifactFamilyHooks,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        for (id, artifact) in self.artifacts.iter() {
            let Some(plan) = &artifact.plan else {
                continue; // `validate_specializations` reports the missing plan.
            };
            let Some(key) = self.specializations.artifact(artifact.ordinal) else {
                continue; // `validate_specializations` reports the missing key.
            };
            let owner = format!("generated artifact {}", id.index());
            if hooks.expands_body(key) && !plan.is_expanded() {
                diagnostics.push(Diagnostic::new(
                    artifact.origin.span.clone(),
                    format!("{owner} still carries the request-time plan marker after closure"),
                ));
                continue;
            }
            if !plan.supports_planned_callees() {
                // Stage 4.5-4.6 families still carry raw requests on the
                // artifact; their plan schema gains callee slots when their
                // own substage lands.
                continue;
            }
            if !plan.is_expanded() {
                // A request-time marker names no callees yet. A registered
                // expander that leaves the marker in place is reported above,
                // so a marker here belongs to a family whose expander has not
                // landed and whose artifact-owned edges stay request-based.
                continue;
            }

            let mut expected_instances = Vec::new();
            let mut expected_artifacts = Vec::new();
            for callee in plan.planned_callees() {
                match callee {
                    PlannedCalleeRef::Instance(planned) => match planned.instance {
                        Some(instance) => {
                            let bound_correctly =
                                self.instances.get(instance).is_some_and(|record| {
                                    self.specializations.instance(record.ordinal)
                                        == Some(&planned.key)
                                });
                            if !bound_correctly {
                                diagnostics.push(Diagnostic::new(
                                    artifact.origin.span.clone(),
                                    format!(
                                        "{owner} binds a planned instance callee to instance {} whose key is not the planned key",
                                        instance.index()
                                    ),
                                ));
                            }
                            expected_instances.push((instance.index(), planned.kind.description()));
                        }
                        None => diagnostics.push(Diagnostic::new(
                            artifact.origin.span.clone(),
                            format!("{owner} has an unbound planned instance callee"),
                        )),
                    },
                    PlannedCalleeRef::Artifact(planned) => match planned.artifact {
                        Some(ordinal) => {
                            if self.specializations.artifact(ordinal) != Some(&planned.key) {
                                diagnostics.push(Diagnostic::new(
                                    artifact.origin.span.clone(),
                                    format!(
                                        "{owner} binds a planned artifact callee to artifact {} whose key is not the planned key",
                                        ordinal.index()
                                    ),
                                ));
                            }
                            expected_artifacts.push((ordinal.index(), planned.kind.description()));
                        }
                        None => diagnostics.push(Diagnostic::new(
                            artifact.origin.span.clone(),
                            format!("{owner} has an unbound planned artifact callee"),
                        )),
                    },
                }
            }

            let actual_instances = artifact
                .instances
                .iter()
                .map(|edge| (edge.instance.index(), edge.kind.description()))
                .collect::<Vec<_>>();
            let actual_artifacts = artifact
                .artifacts
                .iter()
                .map(|edge| (edge.artifact.index(), edge.kind.description()))
                .collect::<Vec<_>>();
            agree_callees_with_edges(
                &owner,
                "instance",
                &expected_instances,
                &actual_instances,
                diagnostics,
            );
            agree_callees_with_edges(
                &owner,
                "artifact",
                &expected_artifacts,
                &actual_artifacts,
                diagnostics,
            );
        }
    }

    /// For every instance body and module initializer, the multiset of closure
    /// artifact uses must equal the owner's closure-phase artifact edges, and
    /// the multiset of closure instance uses must equal its closure-phase
    /// instance edges, each by target, kind, and origin. This ties every
    /// scanner-recorded edge to the exact owner site that references it.
    fn check_use_edge_agreement(&self, diagnostics: &mut Vec<Diagnostic>) {
        for (id, instance) in self.instances.iter() {
            let owner = format!("function instance {}", id.index());
            let artifact_edges = instance
                .artifacts
                .iter()
                .filter(|edge| edge.closure_phase)
                .map(|edge| UseEdge::artifact_edge(edge))
                .collect::<Vec<_>>();
            let instance_edges = instance
                .dependencies
                .iter()
                .filter(|edge| edge.closure_phase)
                .map(|edge| UseEdge::instance_edge(edge))
                .collect::<Vec<_>>();
            let Some(body) = &instance.body else {
                if !artifact_edges.is_empty() || !instance_edges.is_empty() {
                    diagnostics.push(Diagnostic::new(
                        instance.origin.span.clone(),
                        format!("{owner} has closure use records but no materialized body"),
                    ));
                }
                continue;
            };
            let artifact_uses = body
                .artifact_uses
                .iter()
                .map(|use_| (use_.site, UseEdge::artifact_use(use_)))
                .collect::<Vec<_>>();
            let instance_uses = body
                .instance_uses
                .iter()
                .map(|use_| (use_.site, UseEdge::instance_use(use_)))
                .collect::<Vec<_>>();
            let site_owner = UseSiteOwner::Instance(body);
            self.agree_uses_with_edges(
                &owner,
                site_owner,
                "artifact",
                artifact_uses,
                artifact_edges,
                diagnostics,
            );
            self.agree_uses_with_edges(
                &owner,
                site_owner,
                "instance",
                instance_uses,
                instance_edges,
                diagnostics,
            );
        }

        let initializers = self.initializers.len();
        if self.initializer_artifacts.len() != initializers
            || self.initializer_artifact_uses.len() != initializers
            || self.initializer_instances.len() != initializers
            || self.initializer_instance_uses.len() != initializers
        {
            diagnostics.push(Diagnostic::new(
                Span::Compiler,
                "initializer artifact closure storage is not sized for the initializer catalog",
            ));
            return;
        }
        for (id, _) in self.initializers.iter() {
            let owner = format!("initializer {}", id.index());
            let artifact_uses = self.initializer_artifact_uses[id.index()]
                .iter()
                .map(|use_| (use_.site, UseEdge::artifact_use(use_)))
                .collect::<Vec<_>>();
            let artifact_edges = self.initializer_artifacts[id.index()]
                .iter()
                .map(UseEdge::artifact_edge)
                .collect::<Vec<_>>();
            let instance_uses = self.initializer_instance_uses[id.index()]
                .iter()
                .map(|use_| (use_.site, UseEdge::instance_use(use_)))
                .collect::<Vec<_>>();
            let instance_edges = self.initializer_instances[id.index()]
                .iter()
                .map(UseEdge::instance_edge)
                .collect::<Vec<_>>();
            let site_owner = UseSiteOwner::Initializer(id);
            self.agree_uses_with_edges(
                &owner,
                site_owner,
                "artifact",
                artifact_uses,
                artifact_edges,
                diagnostics,
            );
            self.agree_uses_with_edges(
                &owner,
                site_owner,
                "instance",
                instance_uses,
                instance_edges,
                diagnostics,
            );
        }
    }

    /// Matches each use to one remaining edge with the same target, kind, and
    /// origin, reporting unmatched uses and edges.
    fn agree_uses_with_edges(
        &self,
        owner: &str,
        site_owner: UseSiteOwner<'_>,
        family: &str,
        uses: Vec<(ArtifactUseSite, UseEdge<'_>)>,
        mut edges: Vec<UseEdge<'_>>,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        for (site, use_) in uses {
            self.check_use_site(owner, site_owner, site, use_.origin, diagnostics);
            match edges.iter().position(|edge| *edge == use_) {
                Some(index) => {
                    edges.remove(index);
                }
                None => diagnostics.push(Diagnostic::new(
                    use_.origin.span.clone(),
                    format!("{owner} has an {family} use with no matching closure edge"),
                )),
            }
        }
        for edge in edges {
            diagnostics.push(Diagnostic::new(
                edge.origin.span.clone(),
                format!("{owner} has a closure {family} edge with no use"),
            ));
        }
    }

    /// Every use site's ID must resolve inside the owning body's own arenas, so
    /// a scanner cannot tie an edge to a site that Stage 5 could not emit from.
    fn check_use_site(
        &self,
        owner: &str,
        site_owner: UseSiteOwner<'_>,
        site: ArtifactUseSite,
        origin: &Origin,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        let mut report = |kind: &str, index: usize| {
            diagnostics.push(Diagnostic::new(
                origin.span.clone(),
                format!("{owner} has an artifact use site {kind} {index} outside its own arenas"),
            ));
        };
        let item = |id: ItemId| match site_owner {
            UseSiteOwner::Instance(body) => body.item(id).is_some(),
            UseSiteOwner::Initializer(_) => self.items.get(id).is_some(),
        };
        let expression = |id: ExpressionId| match site_owner {
            UseSiteOwner::Instance(body) => body.expression(id).is_some(),
            UseSiteOwner::Initializer(_) => self.expressions.get(id).is_some(),
        };
        let pattern = |id: PatternId| match site_owner {
            UseSiteOwner::Instance(body) => body.pattern(id).is_some(),
            UseSiteOwner::Initializer(_) => self.patterns.get(id).is_some(),
        };
        let call = |id: LoweredCallId| match site_owner {
            UseSiteOwner::Instance(body) => body.call(id).is_some(),
            UseSiteOwner::Initializer(_) => self.calls.get(id).is_some(),
        };
        let callable_value = |id: LoweredCallableValueId| match site_owner {
            UseSiteOwner::Instance(body) => body.callable_value(id).is_some(),
            UseSiteOwner::Initializer(_) => self.callable_values.get(id).is_some(),
        };
        let coro = |id: LoweredCoroId| match site_owner {
            UseSiteOwner::Instance(body) => body.coro(id).is_some(),
            UseSiteOwner::Initializer(_) => self.coros.get(id).is_some(),
        };
        let reactive_operation = |id: LoweredReactiveOperationId| match site_owner {
            UseSiteOwner::Instance(body) => body.reactive_operation(id).is_some(),
            UseSiteOwner::Initializer(_) => self.reactive_operations.get(id).is_some(),
        };
        let reactive_callback = |id: LoweredReactiveCallbackId| match site_owner {
            UseSiteOwner::Instance(body) => body.reactive_callback(id).is_some(),
            UseSiteOwner::Initializer(_) => self.reactive_callbacks.get(id).is_some(),
        };
        match site {
            #[cfg(test)]
            ArtifactUseSite::Test(_) => {}
            ArtifactUseSite::DiscardedResult(id) => {
                if !item(id) {
                    report("item", id.index());
                }
            }
            ArtifactUseSite::ReplacedValue(id) => {
                if !item(id) {
                    report("item", id.index());
                }
            }
            ArtifactUseSite::LoopBodyResult(id) => {
                if !expression(id) {
                    report("expression", id.index());
                }
            }
            ArtifactUseSite::CallTemporary { call: id, .. } => {
                if !call(id) {
                    report("call", id.index());
                }
            }
            ArtifactUseSite::CStringTemporary(id) => {
                if !call(id) {
                    report("call", id.index());
                }
            }
            ArtifactUseSite::WildcardDiscard(id) => {
                if !pattern(id) {
                    report("pattern", id.index());
                }
            }
            ArtifactUseSite::OwnedBinding(symbol) => {
                if !self.symbols.get(symbol).is_some_and(|record| {
                    !matches!(
                        record.storage,
                        crate::SymbolStorage::GlobalStorage
                            | crate::SymbolStorage::FunctionBinding
                            | crate::SymbolStorage::ExternalSymbol
                    )
                }) {
                    report("symbol", symbol.0);
                }
            }
            ArtifactUseSite::CellFinalizer(symbol) => {
                if !self.symbols.get(symbol).is_some_and(|record| {
                    !matches!(
                        record.storage,
                        crate::SymbolStorage::GlobalStorage
                            | crate::SymbolStorage::FunctionBinding
                            | crate::SymbolStorage::ExternalSymbol
                    )
                }) {
                    report("symbol", symbol.0);
                }
            }
            ArtifactUseSite::ClosureEnvironment(id) => {
                if !callable_value(id) {
                    report("callable value", id.index());
                }
            }
            ArtifactUseSite::RefConstruction(id) => {
                if !call(id) {
                    report("call", id.index());
                }
            }
            ArtifactUseSite::DropIntrinsic(id) => {
                if !call(id) {
                    report("call", id.index());
                }
            }
            ArtifactUseSite::CStringConversion(id) => {
                if !call(id) {
                    report("call", id.index());
                }
            }
            ArtifactUseSite::CompletionOrphan(id) => {
                if !call(id) {
                    report("call", id.index());
                }
            }
            ArtifactUseSite::BufferAllocation(id) => {
                if !call(id) {
                    report("call", id.index());
                }
            }
            ArtifactUseSite::BufferCloneFinalizer(id) => {
                if !call(id) {
                    report("call", id.index());
                }
            }
            ArtifactUseSite::BufferCloneElement(id) => {
                if !call(id) {
                    report("call", id.index());
                }
            }
            ArtifactUseSite::CoroCreation(id) => {
                if !coro(id) {
                    report("coro", id.index());
                }
            }
            ArtifactUseSite::ReactiveCallbackEnvironment(id) => {
                if !reactive_callback(id) {
                    report("reactive callback", id.index());
                }
            }
            ArtifactUseSite::DerivedEvaluatorEnvironment(id)
            | ArtifactUseSite::ReactiveRunner(id) => {
                if !reactive_operation(id) {
                    report("reactive operation", id.index());
                }
            }
            ArtifactUseSite::ExternAdapterValue(id) => {
                if !callable_value(id) {
                    report("callable value", id.index());
                }
            }
        }
    }

    /// Following `request` roots from any instance or artifact must terminate
    /// at an initializer, an eager template, or a Stage 3 instance root.
    /// Recursion is fine in edges but not in roots.
    fn check_request_root_acyclicity(&self, diagnostics: &mut Vec<Diagnostic>) {
        let limit = self.instances.len() + self.artifacts.len() + 1;
        for (id, instance) in self.instances.iter() {
            if self.request_root_cycles(RequestRoot::Instance(id), limit) {
                diagnostics.push(Diagnostic::new(
                    instance.origin.span.clone(),
                    format!("function instance {} has a cyclic request root", id.index()),
                ));
            }
        }
        for (id, artifact) in self.artifacts.iter() {
            if self.request_root_cycles(RequestRoot::Artifact(id), limit) {
                diagnostics.push(Diagnostic::new(
                    artifact.origin.span.clone(),
                    format!(
                        "generated artifact {} has a cyclic request root",
                        id.index()
                    ),
                ));
            }
        }
    }

    fn request_root_cycles(&self, start: RequestRoot, limit: usize) -> bool {
        let mut node = start;
        for _ in 0..=limit {
            match node {
                RequestRoot::Instance(id) => {
                    let Some(record) = self.instances.get(id) else {
                        return false;
                    };
                    match &record.request {
                        LoweredInstanceRequest::Initializer { .. }
                        | LoweredInstanceRequest::EagerTemplate => return false,
                        LoweredInstanceRequest::Dependency { owner, .. } => {
                            node = RequestRoot::Instance(*owner);
                        }
                        LoweredInstanceRequest::Artifact { artifact, .. } => {
                            node = RequestRoot::Artifact(*artifact);
                        }
                        LoweredInstanceRequest::Scan { owner, .. } => match owner {
                            LoweredScanOwner::Initializer(_) => return false,
                            LoweredScanOwner::Instance(owner) => {
                                node = RequestRoot::Instance(*owner);
                            }
                        },
                    }
                }
                RequestRoot::Artifact(id) => {
                    let Some(record) = self.artifacts.get(id) else {
                        return false;
                    };
                    match &record.request {
                        LoweredArtifactRequestRoot::Initializer { .. } => return false,
                        LoweredArtifactRequestRoot::Instance { instance, .. } => {
                            node = RequestRoot::Instance(*instance);
                        }
                        LoweredArtifactRequestRoot::Artifact { artifact, .. } => {
                            node = RequestRoot::Artifact(*artifact);
                        }
                    }
                }
            }
        }
        true
    }

    /// Re-runs every scanner and expander read-only against the final program
    /// and requires each returned request to name an already-interned key with
    /// an already-recorded owner edge. This proves the catalog is a fixed
    /// point and catches nondeterministic or order-sensitive hooks.
    fn check_closure_fixed_point(
        &self,
        hooks: &dyn ArtifactFamilyHooks,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        for (id, _) in self.initializers.iter() {
            match hooks.scan_initializer(self, id) {
                Ok(requests) => self.check_fixed_point_requests(
                    FixedPointOwner::Initializer(id),
                    &requests,
                    diagnostics,
                ),
                Err(mut problems) => diagnostics.append(&mut problems),
            }
        }
        for (id, instance) in self.instances.iter() {
            // Scanners read materialized bodies only.
            if instance.body.is_none() {
                continue;
            }
            match hooks.scan_instance(self, id) {
                Ok(requests) => self.check_fixed_point_requests(
                    FixedPointOwner::Instance(id),
                    &requests,
                    diagnostics,
                ),
                Err(mut problems) => diagnostics.append(&mut problems),
            }
        }
        for (id, artifact) in self.artifacts.iter() {
            match hooks.expand(self, id) {
                Ok((plan, requests)) => {
                    // A re-expansion must rebuild the same plan shape modulo
                    // the catalog ids the binding pass filled in. A mismatch
                    // means the expander reads catalog state or is
                    // nondeterministic, which the fixed-point contract
                    // forbids.
                    if let Some(stored) = &artifact.plan
                        && !plan.eq_ignoring_bindings(stored)
                    {
                        diagnostics.push(Diagnostic::new(
                            artifact.origin.span.clone(),
                            format!(
                                "closure did not reach a fixed point: artifact {} re-expanded to a different plan",
                                id.index()
                            ),
                        ));
                    }
                    self.check_fixed_point_requests(
                        FixedPointOwner::Artifact(id),
                        &requests,
                        diagnostics,
                    )
                }
                Err(mut problems) => diagnostics.append(&mut problems),
            }
        }
    }

    fn check_fixed_point_requests(
        &self,
        owner: FixedPointOwner,
        requests: &[ClosureRequest],
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        for request in requests {
            match request {
                ClosureRequest::Instance {
                    resolved,
                    kind,
                    origin,
                    use_site,
                } => {
                    let Some(ordinal) = self.specializations.instance_ordinal(&resolved.key) else {
                        diagnostics.push(Diagnostic::new(
                            origin.span.clone(),
                            format!(
                                "closure did not reach a fixed point: {} requested an uninterned function instance",
                                owner.describe()
                            ),
                        ));
                        continue;
                    };
                    let instance = FunctionInstanceId::from_index(ordinal.index());
                    if !self.owner_records_instance(owner, instance, *kind, origin) {
                        diagnostics.push(Diagnostic::new(
                            origin.span.clone(),
                            format!(
                                "closure did not reach a fixed point: {} has no edge to requested instance {}",
                                owner.describe(),
                                instance.index()
                            ),
                        ));
                    }
                    if let Some(site) = use_site
                        && !self.owner_uses_instance_at(owner, *site, instance, *kind)
                    {
                        diagnostics.push(Diagnostic::new(
                            origin.span.clone(),
                            format!(
                                "{} site {site:?} is not bound to requested instance {}",
                                owner.describe(),
                                instance.index()
                            ),
                        ));
                    }
                }
                ClosureRequest::Artifact {
                    key,
                    kind,
                    origin,
                    use_site,
                    ..
                } => {
                    let Some(ordinal) = self.specializations.artifact_ordinal(key) else {
                        diagnostics.push(Diagnostic::new(
                            origin.span.clone(),
                            format!(
                                "closure did not reach a fixed point: {} requested an uninterned generated artifact",
                                owner.describe()
                            ),
                        ));
                        continue;
                    };
                    if !self.owner_records_artifact(owner, ordinal, *kind, origin) {
                        diagnostics.push(Diagnostic::new(
                            origin.span.clone(),
                            format!(
                                "closure did not reach a fixed point: {} has no edge to requested artifact {}",
                                owner.describe(),
                                ordinal.index()
                            ),
                        ));
                    }
                    if let Some(site) = use_site
                        && !self.owner_uses_artifact_at(owner, *site, ordinal, *kind)
                    {
                        diagnostics.push(Diagnostic::new(
                            origin.span.clone(),
                            format!(
                                "{} site {site:?} is not bound to requested artifact {}",
                                owner.describe(),
                                ordinal.index()
                            ),
                        ));
                    }
                }
            }
        }
    }

    /// Whether the owner's use records bind `site` to exactly this instance
    /// and kind. Edge agreement alone cannot prove this: two sites requesting
    /// the same target with the same kind and origin share one edge shape, so
    /// only the use record ties the reference to the site Stage 5 emits it
    /// from. Artifact owners carry no use records.
    fn owner_uses_instance_at(
        &self,
        owner: FixedPointOwner,
        site: ArtifactUseSite,
        instance: FunctionInstanceId,
        kind: LoweredInstanceDependencyKind,
    ) -> bool {
        let uses: &[LoweredInstanceUse] = match owner {
            FixedPointOwner::Initializer(initializer) => self
                .initializer_instance_uses
                .get(initializer.index())
                .map(Vec::as_slice)
                .unwrap_or_default(),
            FixedPointOwner::Instance(owner) => self
                .instances
                .get(owner)
                .and_then(|record| record.body.as_ref())
                .map(|body| body.instance_uses.as_slice())
                .unwrap_or_default(),
            FixedPointOwner::Artifact(_) => return true,
        };
        uses.iter()
            .any(|use_| use_.site == site && use_.instance == instance && use_.kind == kind)
    }

    /// Whether the owner's use records bind `site` to exactly this artifact
    /// and kind.
    fn owner_uses_artifact_at(
        &self,
        owner: FixedPointOwner,
        site: ArtifactUseSite,
        artifact: ArtifactOrdinal,
        kind: LoweredArtifactDependencyKind,
    ) -> bool {
        let uses: &[LoweredArtifactUse] = match owner {
            FixedPointOwner::Initializer(initializer) => self
                .initializer_artifact_uses
                .get(initializer.index())
                .map(Vec::as_slice)
                .unwrap_or_default(),
            FixedPointOwner::Instance(owner) => self
                .instances
                .get(owner)
                .and_then(|record| record.body.as_ref())
                .map(|body| body.artifact_uses.as_slice())
                .unwrap_or_default(),
            FixedPointOwner::Artifact(_) => return true,
        };
        uses.iter()
            .any(|use_| use_.site == site && use_.artifact == artifact && use_.kind == kind)
    }

    fn owner_records_instance(
        &self,
        owner: FixedPointOwner,
        instance: FunctionInstanceId,
        kind: LoweredInstanceDependencyKind,
        origin: &Origin,
    ) -> bool {
        match owner {
            FixedPointOwner::Initializer(initializer) => self
                .initializer_instances
                .get(initializer.index())
                .is_some_and(|edges| {
                    edges.iter().any(|edge| {
                        edge.instance == instance && edge.kind == kind && edge.origin == *origin
                    })
                }),
            FixedPointOwner::Instance(owner) => self.instances.get(owner).is_some_and(|record| {
                record.dependencies.iter().any(|edge| {
                    edge.instance == instance && edge.kind == kind && edge.origin == *origin
                })
            }),
            FixedPointOwner::Artifact(owner) => self.artifacts.get(owner).is_some_and(|record| {
                record.instances.iter().any(|edge| {
                    edge.instance == instance && edge.kind == kind && edge.origin == *origin
                })
            }),
        }
    }

    fn owner_records_artifact(
        &self,
        owner: FixedPointOwner,
        artifact: ArtifactOrdinal,
        kind: LoweredArtifactDependencyKind,
        origin: &Origin,
    ) -> bool {
        match owner {
            FixedPointOwner::Initializer(initializer) => self
                .initializer_artifacts
                .get(initializer.index())
                .is_some_and(|edges| {
                    edges.iter().any(|edge| {
                        edge.artifact == artifact && edge.kind == kind && edge.origin == *origin
                    })
                }),
            FixedPointOwner::Instance(owner) => self.instances.get(owner).is_some_and(|record| {
                record.artifacts.iter().any(|edge| {
                    edge.artifact == artifact && edge.kind == kind && edge.origin == *origin
                })
            }),
            FixedPointOwner::Artifact(owner) => self.artifacts.get(owner).is_some_and(|record| {
                record.artifacts.iter().any(|edge| {
                    edge.artifact == artifact && edge.kind == kind && edge.origin == *origin
                })
            }),
        }
    }
}

/// Compares a plan's ordered callee expectations with an artifact's ordered
/// edges, reporting every missing edge, every extra edge, and every
/// target/kind disagreement at its position.
fn agree_callees_with_edges(
    owner: &str,
    family: &str,
    expected: &[(usize, &'static str)],
    actual: &[(usize, &'static str)],
    diagnostics: &mut Vec<Diagnostic>,
) {
    let shared = expected.len().min(actual.len());
    for index in 0..shared {
        let (expected_target, expected_kind) = expected[index];
        let (actual_target, actual_kind) = actual[index];
        if expected_target != actual_target || expected_kind != actual_kind {
            diagnostics.push(Diagnostic::new(
                Span::Compiler,
                format!(
                    "{owner} planned {family} callee {index} is ({expected_target}, {expected_kind}) but its edge is ({actual_target}, {actual_kind})"
                ),
            ));
        }
    }
    for (target, kind) in &expected[shared..] {
        diagnostics.push(Diagnostic::new(
            Span::Compiler,
            format!("{owner} has a planned {family} callee ({target}, {kind}) with no artifact-owned edge"),
        ));
    }
    for (target, kind) in &actual[shared..] {
        diagnostics.push(Diagnostic::new(
            Span::Compiler,
            format!("{owner} has an artifact-owned {family} edge ({target}, {kind}) with no planned callee"),
        ));
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    use crate::specialization::{
        ArtifactSite, ArtifactSiteOwner, CanonicalType, GcFinalizerKey, ReactiveRunnerKey,
        StructuralMethodKey,
    };
    use crate::{
        CallSubstitutions, CallTypeSubstitution, CheckedType, DebugDelegate, DebugStep,
        DropGlueBody, DropGluePlan, FunctionId, GcFinalizerPlan, InstanceResolutionRequest,
        InstanceResolutionTarget, NameResolver, PlannedArtifact, PlannedCallee, PlannedInstance,
        ProgramLoader, ReactiveRunnerBody, ReactiveRunnerPlan, StructuralBody,
        StructuralMethodPlan, StructuralTraitMethod, TraitId, TraitMethodId, TypeChecker,
        TypedModule, substitute_type,
    };

    use super::*;

    #[derive(Clone, Copy, PartialEq, Eq, Hash)]
    enum HookOwner {
        Initializer,
        Instance,
        Artifact,
    }

    /// A scripted hook set. Each owner's nth call returns the nth script, so a
    /// two-entry script can prove a fixed point on the first call and inject a
    /// fresh key on the re-check.
    #[derive(Default)]
    struct TestHooks {
        initializer_requests: HashMap<usize, Vec<Vec<ClosureRequest>>>,
        instance_requests: HashMap<usize, Vec<Vec<ClosureRequest>>>,
        artifact_requests: HashMap<usize, Vec<Vec<ClosureRequest>>>,
        calls: RefCell<HashMap<(HookOwner, usize), usize>>,
    }

    impl TestHooks {
        fn script(
            &self,
            owner: HookOwner,
            ordinal: usize,
            table: &HashMap<usize, Vec<Vec<ClosureRequest>>>,
        ) -> Vec<ClosureRequest> {
            let call = {
                let mut calls = self.calls.borrow_mut();
                let entry = calls.entry((owner, ordinal)).or_insert(0);
                let current = *entry;
                *entry += 1;
                current
            };
            table
                .get(&ordinal)
                .and_then(|scripts| scripts.get(call))
                .cloned()
                .unwrap_or_default()
        }
    }

    impl ArtifactFamilyHooks for TestHooks {
        fn scan_initializer(
            &self,
            _program: &LoweredProgram,
            initializer: InitializerId,
        ) -> ScanResult {
            Ok(self.script(
                HookOwner::Initializer,
                initializer.index(),
                &self.initializer_requests,
            ))
        }

        fn scan_instance(
            &self,
            _program: &LoweredProgram,
            instance: FunctionInstanceId,
        ) -> ScanResult {
            Ok(self.script(
                HookOwner::Instance,
                instance.index(),
                &self.instance_requests,
            ))
        }

        fn expand(
            &self,
            program: &LoweredProgram,
            artifact: LoweredArtifactRequestId,
        ) -> ExpansionResult {
            let record = program
                .artifacts
                .get(artifact)
                .expect("the engine expands existing artifacts");
            let plan = record.plan.clone().expect("every artifact carries a plan");
            Ok((
                plan,
                self.script(
                    HookOwner::Artifact,
                    artifact.index(),
                    &self.artifact_requests,
                ),
            ))
        }
    }

    fn standard_library_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("compiler crate should have a workspace parent")
            .join("stdlib")
    }

    fn checked_program(source: &str) -> TypedModule {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("compiler crate should have a workspace parent");
        let program = ProgramLoader::new()
            .with_standard_library_root(standard_library_root())
            .load_source(source, root)
            .expect("test source should load");
        let resolved = NameResolver::new()
            .resolve_program(program)
            .expect("test source should resolve");
        TypeChecker::new()
            .check(resolved)
            .expect("test source should type check")
    }

    /// The Stage 3 program: graph built and materialized, closure not run.
    fn stage_three(source: &str) -> LoweredProgram {
        let module = checked_program(source);
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());
        assert!(program.build_specialization_worklist().is_empty());
        assert!(program.materialize_instance_bodies().is_empty());
        program
    }

    fn function_id(program: &LoweredProgram, name: &str) -> FunctionId {
        program
            .functions
            .iter()
            .find(|(_, _, function)| {
                function.name == name || function.name.ends_with(&format!(".{name}"))
            })
            .map(|(_, id, _)| id)
            .unwrap_or_else(|| panic!("no lowered function named {name}"))
    }

    fn identity_instance(program: &LoweredProgram, index: usize) -> FunctionInstanceId {
        let identity = function_id(program, "identity");
        program
            .instances
            .iter()
            .filter(|(_, instance)| instance.template == identity)
            .map(|(id, _)| id)
            .nth(index)
            .expect("the fixture instantiates identity")
    }

    fn instance_origin(program: &LoweredProgram, instance: FunctionInstanceId) -> Origin {
        program
            .instances
            .get(instance)
            .expect("instance")
            .origin
            .clone()
    }

    fn drop_glue_request(
        value_type: CheckedType,
        origin: &Origin,
        site: Option<ArtifactUseSite>,
    ) -> ClosureRequest {
        let canonical =
            CanonicalType::concrete(&value_type, origin).expect("a concrete drop-glue type");
        ClosureRequest::Artifact {
            key: ArtifactRequestKey::DropGlue(canonical),
            plan: LoweredArtifactPlan::DropGlue(DropGluePlan {
                value_type,
                body: DropGlueBody::Unexpanded,
            }),
            kind: LoweredArtifactDependencyKind::DropGlue,
            origin: origin.clone(),
            use_site: site,
        }
    }

    fn finalizer_request(
        value_type: CheckedType,
        origin: &Origin,
        site: Option<ArtifactUseSite>,
    ) -> ClosureRequest {
        let canonical =
            CanonicalType::concrete(&value_type, origin).expect("a concrete payload type");
        ClosureRequest::Artifact {
            key: ArtifactRequestKey::GcFinalizer(GcFinalizerKey::Payload(canonical)),
            plan: LoweredArtifactPlan::GcFinalizer(GcFinalizerPlan::Payload {
                value_type,
                glue: None,
            }),
            kind: LoweredArtifactDependencyKind::GcFinalizer,
            origin: origin.clone(),
            use_site: site,
        }
    }

    fn runner_request(
        owner: ArtifactSiteOwner,
        site: ArtifactSite,
        origin: &Origin,
        use_site: Option<ArtifactUseSite>,
    ) -> ClosureRequest {
        ClosureRequest::Artifact {
            key: ArtifactRequestKey::ReactionRunner(ReactiveRunnerKey { owner, site }),
            plan: LoweredArtifactPlan::ReactionRunner(ReactiveRunnerPlan {
                owner,
                site,
                body: ReactiveRunnerBody::Unexpanded,
            }),
            kind: LoweredArtifactDependencyKind::ReactionRunner,
            origin: origin.clone(),
            use_site,
        }
    }

    fn resolve_root_instance(
        program: &LoweredProgram,
        function_name: &str,
        value_type: CheckedType,
        origin: &Origin,
    ) -> ResolvedInstanceRequest {
        let function = function_id(program, function_name);
        let template = program.functions.get(function).expect("template").clone();
        let parameter = match template.signature.parameter.as_ref() {
            CheckedType::Parameter { id, .. } => *id,
            other => panic!("{function_name} should take a type parameter, got {other:?}"),
        };
        let map = HashMap::from([(parameter, value_type.clone())]);
        let function_type =
            match substitute_type(CheckedType::Function(template.signature.clone()), &map) {
                CheckedType::Function(function_type) => function_type,
                other => panic!("expected a function signature, got {other:?}"),
            };
        program
            .resolve_instance_request(&InstanceResolutionRequest {
                function,
                origin: origin.clone(),
                function_type,
                substitutions: CallSubstitutions {
                    types: vec![CallTypeSubstitution {
                        parameter,
                        value_type,
                    }],
                    effects: Vec::new(),
                },
                evidence: None,
                target: InstanceResolutionTarget::Root,
            })
            .expect("the synthetic instance request resolves")
    }

    fn close(program: &mut LoweredProgram, hooks: &TestHooks) {
        let diagnostics = program.close_artifact_catalog(hooks);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let diagnostics = program.validate_artifact_closure(hooks);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate_specializations().is_empty());
        assert!(program.validate_instance_bodies().is_empty());
        assert!(program.validate_specialization_graph().is_empty());
    }

    fn messages(diagnostics: &[Diagnostic]) -> Vec<&str> {
        diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect()
    }

    const IDENTITY_FIXTURE: &str = concat!(
        "def identity: <T where Copy T> T -> T = value => value\n",
        "let first: I32 = identity 1\n",
    );

    #[test]
    fn closure_chain_records_ordered_edges_roots_and_ordinals() {
        let mut program = stage_three(IDENTITY_FIXTURE);
        let seed = identity_instance(&program, 0);
        let origin = instance_origin(&program, seed);
        let base = program.artifacts.len();
        let a_type = CheckedType::Ref(Box::new(CheckedType::I32));
        let b_type = CheckedType::Ref(Box::new(a_type.clone()));
        let finalizer_type = CheckedType::Ref(Box::new(b_type.clone()));

        let mut hooks = TestHooks::default();
        hooks.instance_requests.insert(
            seed.index(),
            vec![vec![drop_glue_request(
                a_type,
                &origin,
                Some(ArtifactUseSite::Test(0)),
            )]],
        );
        hooks
            .artifact_requests
            .insert(base, vec![vec![drop_glue_request(b_type, &origin, None)]]);
        hooks.artifact_requests.insert(
            base + 1,
            vec![vec![finalizer_request(finalizer_type, &origin, None)]],
        );
        close(&mut program, &hooks);

        assert_eq!(program.artifacts.len(), base + 3);
        let a = LoweredArtifactRequestId::from_index(base);
        let b = LoweredArtifactRequestId::from_index(base + 1);
        let f = LoweredArtifactRequestId::from_index(base + 2);
        let record = |id| program.artifacts.get(id).expect("artifact");
        assert!(record(a).name.starts_with("__staple_drop_glue"));
        assert!(record(b).name.starts_with("__staple_drop_glue"));
        assert!(record(f).name.starts_with("__staple_gc_finalizer_payload"));
        assert!(matches!(
            &record(a).request,
            LoweredArtifactRequestRoot::Instance {
                instance,
                kind: LoweredArtifactDependencyKind::DropGlue,
                ..
            } if *instance == seed
        ));
        assert!(matches!(
            &record(b).request,
            LoweredArtifactRequestRoot::Artifact { artifact, .. } if *artifact == a
        ));
        assert!(matches!(
            &record(f).request,
            LoweredArtifactRequestRoot::Artifact { artifact, .. } if *artifact == b
        ));
        assert!(record(a).artifacts.iter().any(|edge| {
            edge.artifact == record(b).ordinal
                && edge.closure_phase
                && edge.kind == LoweredArtifactDependencyKind::DropGlue
        }));
        assert!(record(b).artifacts.iter().any(|edge| {
            edge.artifact == record(f).ordinal
                && edge.closure_phase
                && edge.kind == LoweredArtifactDependencyKind::GcFinalizer
        }));

        let body = program
            .instances
            .get(seed)
            .and_then(|instance| instance.body.as_ref())
            .expect("the scanned instance has a body");
        assert_eq!(body.artifact_uses.len(), 1);
        assert_eq!(body.artifact_uses[0].artifact, record(a).ordinal);
        assert_eq!(
            body.artifact_uses[0].kind,
            LoweredArtifactDependencyKind::DropGlue
        );
        assert_eq!(body.artifact_uses[0].site, ArtifactUseSite::Test(0));
        assert!(
            program
                .instances
                .get(seed)
                .expect("instance")
                .artifacts
                .iter()
                .any(|edge| { edge.artifact == record(a).ordinal && edge.closure_phase })
        );
    }

    #[test]
    fn closure_expansion_requests_an_instance_and_scans_it_next_round() {
        let mut program = stage_three(IDENTITY_FIXTURE);
        let seed = identity_instance(&program, 0);
        let origin = instance_origin(&program, seed);
        let base_instances = program.instances.len();
        let base_artifacts = program.artifacts.len();
        let new_type = CheckedType::Ref(Box::new(CheckedType::U8));
        let resolved = resolve_root_instance(&program, "identity", new_type.clone(), &origin);

        let mut hooks = TestHooks::default();
        hooks.instance_requests.insert(
            seed.index(),
            vec![vec![drop_glue_request(
                new_type.clone(),
                &origin,
                Some(ArtifactUseSite::Test(0)),
            )]],
        );
        hooks.artifact_requests.insert(
            base_artifacts,
            vec![vec![ClosureRequest::Instance {
                resolved,
                kind: LoweredInstanceDependencyKind::DirectCall,
                origin: origin.clone(),
                use_site: None,
            }]],
        );
        hooks.instance_requests.insert(
            base_instances,
            vec![vec![finalizer_request(
                new_type.clone(),
                &origin,
                Some(ArtifactUseSite::Test(1)),
            )]],
        );
        close(&mut program, &hooks);

        assert_eq!(program.instances.len(), base_instances + 1);
        assert_eq!(program.artifacts.len(), base_artifacts + 2);
        let new_instance = FunctionInstanceId::from_index(base_instances);
        assert!(
            program
                .instances
                .get(new_instance)
                .expect("resumed instance")
                .body
                .is_some()
        );
        assert!(matches!(
            &program.instances.get(new_instance).expect("instance").request,
            LoweredInstanceRequest::Artifact { artifact, .. }
                if artifact.index() == base_artifacts
        ));
        let first_artifact = LoweredArtifactRequestId::from_index(base_artifacts);
        assert!(
            program
                .artifacts
                .get(first_artifact)
                .expect("artifact")
                .instances
                .iter()
                .any(|edge| edge.instance == new_instance)
        );
        let body = program
            .instances
            .get(new_instance)
            .and_then(|instance| instance.body.as_ref())
            .expect("body");
        assert_eq!(body.artifact_uses.len(), 1);
        assert_eq!(body.artifact_uses[0].site, ArtifactUseSite::Test(1));
    }

    #[test]
    fn closure_instance_rescans_dedup_against_the_requesting_artifact() {
        let mut program = stage_three(IDENTITY_FIXTURE);
        let seed = identity_instance(&program, 0);
        let origin = instance_origin(&program, seed);
        let base_instances = program.instances.len();
        let base_artifacts = program.artifacts.len();
        let value_type = CheckedType::Ref(Box::new(CheckedType::I32));
        let resolved = resolve_root_instance(&program, "identity", value_type.clone(), &origin);

        let mut hooks = TestHooks::default();
        hooks.instance_requests.insert(
            seed.index(),
            vec![vec![drop_glue_request(
                value_type.clone(),
                &origin,
                Some(ArtifactUseSite::Test(0)),
            )]],
        );
        hooks.artifact_requests.insert(
            base_artifacts,
            vec![vec![ClosureRequest::Instance {
                resolved,
                kind: LoweredInstanceDependencyKind::DirectCall,
                origin: origin.clone(),
                use_site: None,
            }]],
        );
        // The instance the artifact requested re-requests the same artifact:
        // it dedups to one ordinal with an edge on the new owner.
        hooks.instance_requests.insert(
            base_instances,
            vec![vec![drop_glue_request(
                value_type,
                &origin,
                Some(ArtifactUseSite::Test(1)),
            )]],
        );
        close(&mut program, &hooks);

        assert_eq!(program.artifacts.len(), base_artifacts + 1);
        assert_eq!(program.instances.len(), base_instances + 1);
        let new_instance = FunctionInstanceId::from_index(base_instances);
        let ordinal = program
            .artifacts
            .get(LoweredArtifactRequestId::from_index(base_artifacts))
            .expect("artifact")
            .ordinal;
        let body = program
            .instances
            .get(new_instance)
            .and_then(|instance| instance.body.as_ref())
            .expect("the rescanned instance has a body");
        assert_eq!(body.artifact_uses.len(), 1);
        assert_eq!(body.artifact_uses[0].artifact, ordinal);
        assert!(
            program
                .instances
                .get(new_instance)
                .expect("instance")
                .artifacts
                .iter()
                .any(|edge| edge.artifact == ordinal && edge.closure_phase)
        );
    }

    #[test]
    fn closure_recursion_through_artifacts_dedups_and_records_back_edges() {
        let mut program = stage_three(IDENTITY_FIXTURE);
        let seed = identity_instance(&program, 0);
        let origin = instance_origin(&program, seed);
        let base = program.artifacts.len();
        let a_type = CheckedType::Ref(Box::new(CheckedType::I32));
        let b_type = CheckedType::Ref(Box::new(a_type.clone()));

        let mut hooks = TestHooks::default();
        hooks.instance_requests.insert(
            seed.index(),
            vec![vec![drop_glue_request(
                a_type.clone(),
                &origin,
                Some(ArtifactUseSite::Test(0)),
            )]],
        );
        // A requests itself and B; B requests A again.
        hooks.artifact_requests.insert(
            base,
            vec![vec![
                drop_glue_request(a_type.clone(), &origin, None),
                drop_glue_request(b_type, &origin, None),
            ]],
        );
        hooks.artifact_requests.insert(
            base + 1,
            vec![vec![drop_glue_request(a_type, &origin, None)]],
        );
        close(&mut program, &hooks);

        assert_eq!(program.artifacts.len(), base + 2, "recursion dedups");
        let a = LoweredArtifactRequestId::from_index(base);
        let b = LoweredArtifactRequestId::from_index(base + 1);
        let ordinal_a = program.artifacts.get(a).expect("artifact").ordinal;
        assert!(
            program
                .artifacts
                .get(a)
                .expect("artifact")
                .artifacts
                .iter()
                .any(|edge| edge.artifact == ordinal_a),
            "A records a self back-edge"
        );
        assert!(
            program
                .artifacts
                .get(b)
                .expect("artifact")
                .artifacts
                .iter()
                .any(|edge| edge.artifact == ordinal_a)
        );
    }

    #[test]
    fn closure_dedups_type_keyed_artifacts_across_owners() {
        let source = concat!(
            "def identity: <T where Copy T> T -> T = value => value\n",
            "let first: I32 = identity 1\n",
            "let second: U8 = identity (1 satisfies U8)\n",
        );
        let mut program = stage_three(source);
        let first = identity_instance(&program, 0);
        let second = identity_instance(&program, 1);
        let origin = instance_origin(&program, first);
        let base = program.artifacts.len();

        let mut hooks = TestHooks::default();
        hooks.instance_requests.insert(
            first.index(),
            vec![vec![drop_glue_request(
                CheckedType::I32,
                &origin,
                Some(ArtifactUseSite::Test(0)),
            )]],
        );
        hooks.instance_requests.insert(
            second.index(),
            vec![vec![drop_glue_request(
                CheckedType::I32,
                &origin,
                Some(ArtifactUseSite::Test(1)),
            )]],
        );
        close(&mut program, &hooks);

        assert_eq!(program.artifacts.len(), base + 1, "one shared ordinal");
        let shared = LoweredArtifactRequestId::from_index(base);
        let ordinal = program.artifacts.get(shared).expect("artifact").ordinal;
        let first_body = program
            .instances
            .get(first)
            .and_then(|instance| instance.body.as_ref())
            .expect("body");
        let second_body = program
            .instances
            .get(second)
            .and_then(|instance| instance.body.as_ref())
            .expect("body");
        assert_eq!(first_body.artifact_uses.len(), 1);
        assert_eq!(second_body.artifact_uses.len(), 1);
        assert_eq!(first_body.artifact_uses[0].artifact, ordinal);
        assert_eq!(second_body.artifact_uses[0].artifact, ordinal);
        for (instance, site) in [(first, 0), (second, 1)] {
            let edges = program
                .instances
                .get(instance)
                .expect("instance")
                .artifacts
                .iter()
                .filter(|edge| edge.artifact == ordinal)
                .count();
            assert_eq!(edges, 1, "instance {instance:?} records one edge");
            let use_site = if site == 0 {
                first_body.artifact_uses[0].site
            } else {
                second_body.artifact_uses[0].site
            };
            assert_eq!(use_site, ArtifactUseSite::Test(site as u32));
        }
    }

    #[test]
    fn closure_per_owner_sites_produce_distinct_keys() {
        let source = concat!(
            "def identity: <T where Copy T> T -> T = value => value\n",
            "let first: I32 = identity 1\n",
            "let second: U8 = identity (1 satisfies U8)\n",
        );
        let mut program = stage_three(source);
        let first = identity_instance(&program, 0);
        let second = identity_instance(&program, 1);
        let origin = instance_origin(&program, first);
        let base = program.artifacts.len();

        let first_owner = program.instances.get(first).expect("instance").ordinal;
        let second_owner = program.instances.get(second).expect("instance").ordinal;
        let site = ArtifactSite::PlanLocal(7);
        let mut hooks = TestHooks::default();
        hooks.instance_requests.insert(
            first.index(),
            vec![vec![runner_request(
                ArtifactSiteOwner::Instance(first_owner),
                site,
                &origin,
                Some(ArtifactUseSite::Test(0)),
            )]],
        );
        hooks.instance_requests.insert(
            second.index(),
            vec![vec![runner_request(
                ArtifactSiteOwner::Instance(second_owner),
                site,
                &origin,
                Some(ArtifactUseSite::Test(0)),
            )]],
        );
        close(&mut program, &hooks);

        assert_eq!(
            program.artifacts.len(),
            base + 2,
            "the same site in two owners yields two keys"
        );
        let first_key = program
            .specializations
            .artifact(
                program
                    .artifacts
                    .get(LoweredArtifactRequestId::from_index(base))
                    .expect("artifact")
                    .ordinal,
            )
            .expect("key");
        let second_key = program
            .specializations
            .artifact(
                program
                    .artifacts
                    .get(LoweredArtifactRequestId::from_index(base + 1))
                    .expect("artifact")
                    .ordinal,
            )
            .expect("key");
        assert_ne!(first_key, second_key);
    }

    #[test]
    fn closure_never_renumbers_or_renames_stage_three_entries() {
        let source = concat!(
            "def identity: <T where Copy T> T -> T = value => value\n",
            "let first: I32 = identity 1\n",
            "type Point = ctor (I32, I32)\n",
            "let make: () -> ((I32, I32) -> Point) = () => Point\n",
            "let p = (1, 2)\n",
            "let text = \"${p:?}\"\n",
        );
        let baseline = stage_three(source);
        let baseline_instances = baseline
            .instances
            .iter()
            .map(|(id, instance)| (id.index(), instance.name.clone()))
            .collect::<Vec<_>>();
        let baseline_artifacts = baseline
            .artifacts
            .iter()
            .map(|(id, artifact)| (id.index(), artifact.name.clone()))
            .collect::<Vec<_>>();
        assert!(!baseline_instances.is_empty());
        assert!(!baseline_artifacts.is_empty());

        let mut program = stage_three(source);
        let seed = identity_instance(&program, 0);
        let origin = instance_origin(&program, seed);
        let base = program.artifacts.len();
        let mut hooks = TestHooks::default();
        hooks.instance_requests.insert(
            seed.index(),
            vec![vec![
                drop_glue_request(
                    CheckedType::Ref(Box::new(CheckedType::I32)),
                    &origin,
                    Some(ArtifactUseSite::Test(0)),
                ),
                runner_request(
                    ArtifactSiteOwner::Initializer(InitializerId::from_index(0)),
                    ArtifactSite::PlanLocal(0),
                    &origin,
                    Some(ArtifactUseSite::Test(1)),
                ),
            ]],
        );
        close(&mut program, &hooks);

        let closed_instances = program
            .instances
            .iter()
            .take(baseline_instances.len())
            .map(|(id, instance)| (id.index(), instance.name.clone()))
            .collect::<Vec<_>>();
        assert_eq!(baseline_instances, closed_instances);
        let closed_artifacts = program
            .artifacts
            .iter()
            .take(baseline_artifacts.len())
            .map(|(id, artifact)| (id.index(), artifact.name.clone()))
            .collect::<Vec<_>>();
        assert_eq!(baseline_artifacts, closed_artifacts);
        assert_eq!(program.artifacts.len(), base + 2);
    }

    fn closure_snapshot(program: &LoweredProgram) -> String {
        let mut out = String::new();
        for (id, instance) in program.instances.iter() {
            out.push_str(&format!(
                "instance {} name={} request={:?} deps={:?} artifacts={:?}\n",
                id.index(),
                instance.name,
                instance.request,
                instance
                    .dependencies
                    .iter()
                    .map(|edge| (
                        edge.instance.index(),
                        edge.kind.description(),
                        edge.closure_phase
                    ))
                    .collect::<Vec<_>>(),
                instance
                    .artifacts
                    .iter()
                    .map(|edge| (
                        edge.artifact.index(),
                        edge.kind.description(),
                        edge.closure_phase
                    ))
                    .collect::<Vec<_>>(),
            ));
            if let Some(body) = &instance.body {
                out.push_str(&format!(
                    "  uses={:?} instance_uses={:?} owned={:?}\n",
                    body.artifact_uses
                        .iter()
                        .map(|use_| (use_.artifact.index(), use_.kind.description(), use_.site))
                        .collect::<Vec<_>>(),
                    body.instance_uses
                        .iter()
                        .map(|use_| (use_.instance.index(), use_.kind.description(), use_.site))
                        .collect::<Vec<_>>(),
                    body.owned_bindings
                        .iter()
                        .map(|binding| (
                            binding.symbol.0,
                            binding.storage,
                            binding.glue.map(|glue| glue.index())
                        ))
                        .collect::<Vec<_>>(),
                ));
            }
        }
        for (id, artifact) in program.artifacts.iter() {
            let family = program
                .specializations
                .artifact(artifact.ordinal)
                .map(|key| key.family_name())
                .unwrap_or("<missing>");
            out.push_str(&format!(
                "artifact {} name={} family={family} root={:?} edges={:?} instances={:?} expanded={} plan={:?}\n",
                id.index(),
                artifact.name,
                artifact.request,
                artifact
                    .artifacts
                    .iter()
                    .map(|edge| (edge.artifact.index(), edge.kind.description(), edge.closure_phase))
                    .collect::<Vec<_>>(),
                artifact
                    .instances
                    .iter()
                    .map(|edge| (edge.instance.index(), edge.kind.description(), edge.closure_phase))
                    .collect::<Vec<_>>(),
                artifact.expanded,
                artifact.plan,
            ));
        }
        out
    }

    #[test]
    fn closure_is_deterministic_across_repeated_runs() {
        let source = concat!(
            "def identity: <T where Copy T> T -> T = value => value\n",
            "let first: I32 = identity 1\n",
            "let second: U8 = identity (1 satisfies U8)\n",
        );
        let run = || {
            let mut program = stage_three(source);
            let first = identity_instance(&program, 0);
            let second = identity_instance(&program, 1);
            let origin = instance_origin(&program, first);
            let mut by_type = HashMap::new();
            by_type.insert(0usize, first);
            by_type.insert(1usize, second);
            let mut instance_requests = HashMap::new();
            for (index, instance) in by_type {
                instance_requests.insert(
                    instance.index(),
                    vec![vec![drop_glue_request(
                        if index == 0 {
                            CheckedType::I32
                        } else {
                            CheckedType::U8
                        },
                        &origin,
                        Some(ArtifactUseSite::Test(index as u32)),
                    )]],
                );
            }
            let hooks = TestHooks {
                instance_requests,
                ..TestHooks::default()
            };
            close(&mut program, &hooks);
            closure_snapshot(&program)
        };
        assert_eq!(run(), run());
    }

    /// A hook set that reserves one fresh instance and artifact per round so
    /// the round bound is reached without unbounded per-round work.
    struct GrowHooks {
        seed: FunctionInstanceId,
        origin: Origin,
        baseline_instances: usize,
        baseline_artifacts: usize,
        depth: Cell<u64>,
    }

    impl ArtifactFamilyHooks for GrowHooks {
        fn scan_initializer(
            &self,
            _program: &LoweredProgram,
            _initializer: InitializerId,
        ) -> ScanResult {
            Ok(Vec::new())
        }

        fn scan_instance(
            &self,
            _program: &LoweredProgram,
            instance: FunctionInstanceId,
        ) -> ScanResult {
            if instance == self.seed {
                return Ok(vec![drop_glue_request(
                    grow_type(0),
                    &self.origin,
                    Some(ArtifactUseSite::Test(0)),
                )]);
            }
            if instance.index() >= self.baseline_instances {
                let depth = self.depth.get();
                return Ok(vec![drop_glue_request(
                    grow_type(depth),
                    &self.origin,
                    Some(ArtifactUseSite::Test(depth as u32)),
                )]);
            }
            Ok(Vec::new())
        }

        fn expand(
            &self,
            program: &LoweredProgram,
            artifact: LoweredArtifactRequestId,
        ) -> ExpansionResult {
            let plan = program
                .artifacts
                .get(artifact)
                .expect("artifact")
                .plan
                .clone()
                .expect("plan");
            if artifact.index() < self.baseline_artifacts {
                return Ok((plan, Vec::new()));
            }
            let depth = self.depth.get() + 1;
            self.depth.set(depth);
            let resolved =
                resolve_root_instance(program, "identity", grow_type(depth), &self.origin);
            Ok((
                plan,
                vec![ClosureRequest::Instance {
                    resolved,
                    kind: LoweredInstanceDependencyKind::DirectCall,
                    origin: self.origin.clone(),
                    use_site: None,
                }],
            ))
        }
    }

    /// A distinct concrete type per growth step: an array whose count is the
    /// step number keeps every key small but fresh.
    fn grow_type(depth: u64) -> CheckedType {
        CheckedType::Array {
            element: Box::new(CheckedType::I32),
            count: Box::new(CheckedType::NumberLiteral(depth + 1)),
        }
    }

    #[test]
    fn closure_non_convergence_hits_the_bound_with_a_requester_chain() {
        let mut program = stage_three(IDENTITY_FIXTURE);
        let seed = identity_instance(&program, 0);
        let baseline_instances = program.instances.len();
        let baseline_artifacts = program.artifacts.len();
        let hooks = GrowHooks {
            seed,
            origin: instance_origin(&program, seed),
            baseline_instances,
            baseline_artifacts,
            depth: Cell::new(0),
        };
        let diagnostics = program.close_artifact_catalog(&hooks);
        let messages = messages(&diagnostics);
        assert!(
            messages.iter().any(
                |message| message.contains("did not converge after 64 rounds")
                    || message.contains("grew by")
            ),
            "{messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|message| message.contains("request chain")),
            "{messages:?}"
        );
    }

    #[test]
    fn closure_corruption_is_diagnosed() {
        let source = concat!(
            "def identity: <T where Copy T> T -> T = value => value\n",
            "let first: I32 = identity 1\n",
        );
        let mut program = stage_three(source);
        let seed = identity_instance(&program, 0);
        let origin = instance_origin(&program, seed);
        let base = program.artifacts.len();
        let mut hooks = TestHooks::default();
        hooks.instance_requests.insert(
            seed.index(),
            vec![vec![drop_glue_request(
                CheckedType::Ref(Box::new(CheckedType::I32)),
                &origin,
                Some(ArtifactUseSite::Test(0)),
            )]],
        );
        close(&mut program, &hooks);
        assert_eq!(program.artifacts.len(), base + 1);
        let artifact = LoweredArtifactRequestId::from_index(base);

        // A closure edge whose use was removed.
        let mut broken = program.clone();
        broken
            .instances
            .get_mut(seed)
            .and_then(|instance| instance.body.as_mut())
            .expect("body")
            .artifact_uses
            .clear();
        assert!(
            messages(&broken.validate_artifact_closure(&TestHooks::default()))
                .iter()
                .any(|message| message.contains("closure artifact edge with no use"))
        );

        // An extra use with no edge.
        let mut broken = program.clone();
        let duplicate = broken
            .instances
            .get(seed)
            .and_then(|instance| instance.body.as_ref())
            .expect("body")
            .artifact_uses[0]
            .clone();
        broken
            .instances
            .get_mut(seed)
            .and_then(|instance| instance.body.as_mut())
            .expect("body")
            .artifact_uses
            .push(duplicate);
        assert!(
            messages(&broken.validate_artifact_closure(&TestHooks::default()))
                .iter()
                .any(|message| message.contains("use with no matching closure edge"))
        );

        // A requester whose recorded edge kind disagrees with the root kind.
        let mut broken = program.clone();
        let kind = match &broken.artifacts.get(artifact).expect("artifact").request {
            LoweredArtifactRequestRoot::Instance { kind, .. } => *kind,
            other => panic!("expected an instance root, got {other:?}"),
        };
        assert_eq!(kind, LoweredArtifactDependencyKind::DropGlue);
        if let LoweredArtifactRequestRoot::Instance { kind, .. } = &mut broken
            .artifacts
            .get_mut(artifact)
            .expect("artifact")
            .request
        {
            *kind = LoweredArtifactDependencyKind::GcFinalizer;
        }
        assert!(
            messages(&broken.validate_artifact_closure(&TestHooks::default()))
                .iter()
                .any(|message| message
                    .contains("no recorded edge on the instance that requested it"))
        );

        // An artifact the closure never expanded.
        let mut broken = program.clone();
        broken
            .artifacts
            .get_mut(artifact)
            .expect("artifact")
            .expanded = false;
        assert!(
            messages(&broken.validate_artifact_closure(&TestHooks::default()))
                .iter()
                .any(|message| message.contains("was never expanded"))
        );

        // A plan that no longer rebuilds its key.
        let mut broken = program.clone();
        broken.artifacts.get_mut(artifact).expect("artifact").plan =
            Some(LoweredArtifactPlan::DropGlue(DropGluePlan {
                value_type: CheckedType::U8,
                body: DropGlueBody::Unexpanded,
            }));
        assert!(
            messages(&broken.validate_specializations())
                .iter()
                .any(|message| message.contains("does not rebuild its key"))
        );

        // An instance whose template has a body but no materialized body.
        let mut broken = program.clone();
        broken.instances.get_mut(seed).expect("instance").body = None;
        assert!(
            messages(&broken.validate_instance_bodies())
                .iter()
                .any(|message| message.contains("has no concrete body"))
        );

        // A cyclic request root.
        let mut broken = program.clone();
        let other = broken
            .instances
            .iter()
            .find(|(id, _)| *id != seed)
            .map(|(id, _)| id)
            .expect("a second instance");
        let seed_origin = instance_origin(&broken, seed);
        let other_origin = instance_origin(&broken, other);
        broken.instances.get_mut(seed).expect("instance").request =
            LoweredInstanceRequest::Dependency {
                owner: other,
                kind: LoweredInstanceDependencyKind::DirectCall,
                origin: seed_origin,
            };
        broken.instances.get_mut(other).expect("instance").request =
            LoweredInstanceRequest::Dependency {
                owner: seed,
                kind: LoweredInstanceDependencyKind::DirectCall,
                origin: other_origin,
            };
        assert!(
            messages(&broken.validate_artifact_closure(&TestHooks::default()))
                .iter()
                .any(|message| message.contains("cyclic request root"))
        );
    }

    #[test]
    fn closure_use_site_outside_the_owner_arenas_is_diagnosed() {
        let source = concat!(
            "def identity: <T where Copy T> T -> T = value => value\n",
            "let first: I32 = identity 1\n",
        );
        let mut program = stage_three(source);
        let seed = identity_instance(&program, 0);
        let origin = instance_origin(&program, seed);
        let mut hooks = TestHooks::default();
        hooks.instance_requests.insert(
            seed.index(),
            vec![vec![drop_glue_request(
                CheckedType::Ref(Box::new(CheckedType::I32)),
                &origin,
                // The item position is far outside the owner's body arena.
                Some(ArtifactUseSite::DiscardedResult(ItemId::from_index(
                    1_000_000,
                ))),
            )]],
        );
        let diagnostics = program.close_artifact_catalog(&hooks);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let diagnostics = program.validate_artifact_closure(&hooks);
        assert!(
            messages(&diagnostics)
                .iter()
                .any(|message| message.contains("outside its own arenas")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn closure_stage_4_5_use_sites_outside_the_owner_arenas_are_diagnosed() {
        let source = concat!(
            "def identity: <T where Copy T> T -> T = value => value\n",
            "let first: I32 = identity 1\n",
        );
        // Each request carries a distinct drop-glue key so every use is
        // recorded, and an ID far outside the owner's body arenas.
        let broken = [
            (
                CheckedType::Ref(Box::new(CheckedType::I32)),
                ArtifactUseSite::CoroCreation(LoweredCoroId::from_index(1_000_000)),
            ),
            (
                CheckedType::Ref(Box::new(CheckedType::U8)),
                ArtifactUseSite::ReactiveCallbackEnvironment(
                    LoweredReactiveCallbackId::from_index(1_000_000),
                ),
            ),
            (
                CheckedType::Ref(Box::new(CheckedType::U16)),
                ArtifactUseSite::DerivedEvaluatorEnvironment(
                    LoweredReactiveOperationId::from_index(1_000_000),
                ),
            ),
            (
                CheckedType::Ref(Box::new(CheckedType::I64)),
                ArtifactUseSite::ReactiveRunner(LoweredReactiveOperationId::from_index(1_000_000)),
            ),
        ];
        for (value_type, site) in broken {
            let mut program = stage_three(source);
            let seed = identity_instance(&program, 0);
            let origin = instance_origin(&program, seed);
            let mut hooks = TestHooks::default();
            hooks.instance_requests.insert(
                seed.index(),
                vec![vec![drop_glue_request(value_type, &origin, Some(site))]],
            );
            let diagnostics = program.close_artifact_catalog(&hooks);
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
            let diagnostics = program.validate_artifact_closure(&hooks);
            assert!(
                messages(&diagnostics)
                    .iter()
                    .any(|message| message.contains("outside its own arenas")),
                "the {site:?} site is diagnosed: {diagnostics:?}"
            );
        }
    }

    #[test]
    fn closure_fixed_point_violation_is_diagnosed() {
        let mut program = stage_three(IDENTITY_FIXTURE);
        let seed = identity_instance(&program, 0);
        let origin = instance_origin(&program, seed);
        let base = program.artifacts.len();
        let mut hooks = TestHooks::default();
        hooks.instance_requests.insert(
            seed.index(),
            vec![vec![drop_glue_request(
                CheckedType::Ref(Box::new(CheckedType::I32)),
                &origin,
                Some(ArtifactUseSite::Test(0)),
            )]],
        );
        // The first expansion reserves nothing; the re-check invents a key.
        hooks.artifact_requests.insert(
            base,
            vec![
                Vec::new(),
                vec![drop_glue_request(
                    CheckedType::Ref(Box::new(CheckedType::U8)),
                    &origin,
                    None,
                )],
            ],
        );
        let diagnostics = program.close_artifact_catalog(&hooks);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let diagnostics = program.validate_artifact_closure(&hooks);
        assert!(
            messages(&diagnostics)
                .iter()
                .any(|message| message.contains("did not reach a fixed point")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn closure_scan_and_expansion_requests_from_initializers_are_recorded() {
        let mut program = stage_three("let value = 1\n");
        let initializer = InitializerId::from_index(0);
        let origin = program
            .initializers
            .get(initializer)
            .expect("initializer")
            .origin
            .clone();
        let base = program.artifacts.len();
        let mut hooks = TestHooks::default();
        hooks.initializer_requests.insert(
            0,
            vec![vec![drop_glue_request(
                CheckedType::Ref(Box::new(CheckedType::I32)),
                &origin,
                Some(ArtifactUseSite::Test(4)),
            )]],
        );
        close(&mut program, &hooks);

        assert_eq!(program.artifacts.len(), base + 1);
        let artifact = LoweredArtifactRequestId::from_index(base);
        assert!(matches!(
            &program.artifacts.get(artifact).expect("artifact").request,
            LoweredArtifactRequestRoot::Initializer { initializer: owner, .. } if *owner == initializer
        ));
        assert_eq!(program.initializer_artifacts[0].len(), 1);
        assert_eq!(program.initializer_artifacts[0][0].artifact.index(), base);
        assert!(program.initializer_artifacts[0][0].closure_phase);
        assert_eq!(program.initializer_artifact_uses[0].len(), 1);
        assert_eq!(
            program.initializer_artifact_uses[0][0].site,
            ArtifactUseSite::Test(4)
        );
    }

    /// Hooks whose expansions only ever append fresh artifacts, never an
    /// instance, so the round boundary is never reached.
    struct ArtifactOnlyGrowHooks {
        seed: FunctionInstanceId,
        origin: Origin,
        baseline_artifacts: usize,
    }

    impl ArtifactFamilyHooks for ArtifactOnlyGrowHooks {
        fn scan_initializer(
            &self,
            _program: &LoweredProgram,
            _initializer: InitializerId,
        ) -> ScanResult {
            Ok(Vec::new())
        }

        fn scan_instance(
            &self,
            _program: &LoweredProgram,
            instance: FunctionInstanceId,
        ) -> ScanResult {
            if instance == self.seed {
                return Ok(vec![drop_glue_request(
                    grow_type(0),
                    &self.origin,
                    Some(ArtifactUseSite::Test(0)),
                )]);
            }
            Ok(Vec::new())
        }

        fn expand(
            &self,
            program: &LoweredProgram,
            artifact: LoweredArtifactRequestId,
        ) -> ExpansionResult {
            let plan = program
                .artifacts
                .get(artifact)
                .expect("artifact")
                .plan
                .clone()
                .expect("plan");
            if artifact.index() < self.baseline_artifacts {
                return Ok((plan, Vec::new()));
            }
            let depth = (artifact.index() - self.baseline_artifacts + 1) as u64;
            Ok((
                plan,
                vec![drop_glue_request(grow_type(depth), &self.origin, None)],
            ))
        }
    }

    #[test]
    fn closure_artifact_only_growth_hits_the_budget_instead_of_hanging() {
        let mut program = stage_three(IDENTITY_FIXTURE);
        let seed = identity_instance(&program, 0);
        let baseline_instances = program.instances.len();
        let hooks = ArtifactOnlyGrowHooks {
            seed,
            origin: instance_origin(&program, seed),
            baseline_artifacts: program.artifacts.len(),
        };
        let diagnostics = program.close_artifact_catalog(&hooks);
        let messages = messages(&diagnostics);
        assert!(
            messages
                .iter()
                .any(|message| message.contains("grew by") && message.contains("request chain")),
            "{messages:?}"
        );
        assert_eq!(
            program.instances.len(),
            baseline_instances,
            "the chain never requested an instance"
        );
    }

    #[test]
    fn closure_scanner_instance_requests_record_their_use_sites() {
        let mut program = stage_three(IDENTITY_FIXTURE);
        let seed = identity_instance(&program, 0);
        let origin = instance_origin(&program, seed);
        let initializer_origin = program
            .initializers
            .get(InitializerId::from_index(0))
            .expect("initializer")
            .origin
            .clone();
        let base_instances = program.instances.len();
        let from_instance = resolve_root_instance(
            &program,
            "identity",
            CheckedType::Ref(Box::new(CheckedType::U8)),
            &origin,
        );
        let from_initializer = resolve_root_instance(
            &program,
            "identity",
            CheckedType::Ref(Box::new(CheckedType::U16)),
            &initializer_origin,
        );

        let mut hooks = TestHooks::default();
        let instance_request = ClosureRequest::Instance {
            resolved: from_instance,
            kind: LoweredInstanceDependencyKind::DirectCall,
            origin: origin.clone(),
            use_site: Some(ArtifactUseSite::Test(2)),
        };
        let initializer_request = ClosureRequest::Instance {
            resolved: from_initializer,
            kind: LoweredInstanceDependencyKind::DirectCall,
            origin: initializer_origin.clone(),
            use_site: Some(ArtifactUseSite::Test(3)),
        };
        hooks.instance_requests.insert(
            seed.index(),
            vec![vec![instance_request.clone()], vec![instance_request]],
        );
        hooks.initializer_requests.insert(
            0,
            vec![vec![initializer_request.clone()], vec![initializer_request]],
        );
        close(&mut program, &hooks);

        assert_eq!(program.instances.len(), base_instances + 2);
        let from_initializer = FunctionInstanceId::from_index(base_instances);
        let from_instance = FunctionInstanceId::from_index(base_instances + 1);

        let seed_record = program.instances.get(seed).expect("seed");
        let uses = &seed_record.body.as_ref().expect("seed body").instance_uses;
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].site, ArtifactUseSite::Test(2));
        assert_eq!(uses[0].instance, from_instance);
        assert!(
            seed_record
                .dependencies
                .iter()
                .any(|edge| { edge.instance == from_instance && edge.closure_phase })
        );

        assert_eq!(program.initializer_instances[0].len(), 1);
        assert_eq!(
            program.initializer_instances[0][0].instance,
            from_initializer
        );
        assert!(program.initializer_instances[0][0].closure_phase);
        assert_eq!(program.initializer_instance_uses[0].len(), 1);
        assert_eq!(
            program.initializer_instance_uses[0][0].site,
            ArtifactUseSite::Test(3)
        );
    }

    #[test]
    fn closure_scanner_instance_edges_without_a_use_site_are_diagnosed() {
        let mut program = stage_three(IDENTITY_FIXTURE);
        let seed = identity_instance(&program, 0);
        let origin = instance_origin(&program, seed);
        let resolved = resolve_root_instance(
            &program,
            "identity",
            CheckedType::Ref(Box::new(CheckedType::U8)),
            &origin,
        );
        let request = ClosureRequest::Instance {
            resolved,
            kind: LoweredInstanceDependencyKind::DirectCall,
            origin: origin.clone(),
            use_site: None,
        };
        let mut hooks = TestHooks::default();
        hooks
            .instance_requests
            .insert(seed.index(), vec![vec![request.clone()], vec![request]]);
        assert!(program.close_artifact_catalog(&hooks).is_empty());
        let diagnostics = program.validate_artifact_closure(&hooks);
        assert!(
            messages(&diagnostics)
                .iter()
                .any(|message| message.contains("has a closure instance edge with no use")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn closure_expander_instance_requests_reject_use_sites() {
        let mut program = stage_three(IDENTITY_FIXTURE);
        let seed = identity_instance(&program, 0);
        let origin = instance_origin(&program, seed);
        let base = program.artifacts.len();
        let resolved = resolve_root_instance(
            &program,
            "identity",
            CheckedType::Ref(Box::new(CheckedType::U8)),
            &origin,
        );
        let mut hooks = TestHooks::default();
        hooks.instance_requests.insert(
            seed.index(),
            vec![vec![drop_glue_request(
                CheckedType::Ref(Box::new(CheckedType::I32)),
                &origin,
                Some(ArtifactUseSite::Test(0)),
            )]],
        );
        hooks.artifact_requests.insert(
            base,
            vec![vec![ClosureRequest::Instance {
                resolved,
                kind: LoweredInstanceDependencyKind::DirectCall,
                origin: origin.clone(),
                use_site: Some(ArtifactUseSite::Test(1)),
            }]],
        );
        let diagnostics = program.close_artifact_catalog(&hooks);
        assert!(
            messages(&diagnostics)
                .iter()
                .any(|message| message
                    .contains("expander-produced instance requests carry no use site")),
            "{diagnostics:?}"
        );
    }

    /// Builds a complete `StructuralMethod` plan whose `ProductDebug` body
    /// names a `Formatter.write` instance and one artifact delegate, together
    /// with the matching key.
    fn planned_debug_plan(
        program: &LoweredProgram,
        seed: FunctionInstanceId,
        origin: &Origin,
        write: PlannedInstance,
        delegate: PlannedCallee,
    ) -> (ArtifactRequestKey, LoweredArtifactPlan) {
        let callable_type = program
            .instances
            .get(seed)
            .and_then(|instance| instance.body.as_ref())
            .expect("the fixture seed has a materialized body")
            .signature
            .clone();
        let key = ArtifactRequestKey::StructuralMethod(
            StructuralMethodKey::new(
                StructuralTraitMethod::Debug,
                TraitId(3),
                TraitMethodId(4),
                &[CheckedType::I32],
                &callable_type,
                origin,
            )
            .expect("the synthetic structural key is concrete"),
        );
        let plan = LoweredArtifactPlan::StructuralMethod(StructuralMethodPlan {
            structural: StructuralTraitMethod::Debug,
            trait_id: TraitId(3),
            method: TraitMethodId(4),
            arguments: vec![CheckedType::I32],
            callable_type: callable_type.clone(),
            body: StructuralBody::ProductDebug {
                steps: vec![DebugStep::Element {
                    index: 0,
                    delegate: DebugDelegate {
                        value_type: CheckedType::I32,
                        callee: delegate,
                        callee_type: callable_type.clone(),
                    },
                }],
                write,
            },
        });
        (key, plan)
    }

    #[test]
    fn closure_binds_planned_callees_after_the_fixed_point() {
        let mut program = stage_three(IDENTITY_FIXTURE);
        let seed = identity_instance(&program, 0);
        let origin = instance_origin(&program, seed);
        let base_instances = program.instances.len();
        let base_artifacts = program.artifacts.len();
        let planned_type = CheckedType::Ref(Box::new(CheckedType::U8));
        let resolved = resolve_root_instance(&program, "identity", planned_type, &origin);
        let write = PlannedInstance {
            key: resolved.key.clone(),
            instance: None,
            kind: LoweredInstanceDependencyKind::FormattingWrite,
        };
        let drop_type = CheckedType::Ref(Box::new(CheckedType::I32));
        let drop_key = ArtifactRequestKey::DropGlue(
            CanonicalType::concrete(&drop_type, &origin).expect("type"),
        );
        let delegate = PlannedCallee::Artifact(PlannedArtifact {
            key: drop_key.clone(),
            artifact: None,
            kind: LoweredArtifactDependencyKind::DropGlue,
        });
        let (key, plan) = planned_debug_plan(&program, seed, &origin, write, delegate);

        let mut hooks = TestHooks::default();
        hooks.instance_requests.insert(
            seed.index(),
            vec![vec![ClosureRequest::Artifact {
                key,
                plan,
                kind: LoweredArtifactDependencyKind::StructuralMethod,
                origin: origin.clone(),
                use_site: Some(ArtifactUseSite::Test(0)),
            }]],
        );
        hooks.artifact_requests.insert(
            base_artifacts,
            vec![vec![
                ClosureRequest::Instance {
                    resolved,
                    kind: LoweredInstanceDependencyKind::FormattingWrite,
                    origin: origin.clone(),
                    use_site: None,
                },
                ClosureRequest::Artifact {
                    key: drop_key,
                    plan: LoweredArtifactPlan::DropGlue(DropGluePlan {
                        value_type: drop_type,
                        body: DropGlueBody::Unexpanded,
                    }),
                    kind: LoweredArtifactDependencyKind::DropGlue,
                    origin: origin.clone(),
                    use_site: None,
                },
            ]],
        );
        close(&mut program, &hooks);

        assert_eq!(program.instances.len(), base_instances + 1);
        assert_eq!(program.artifacts.len(), base_artifacts + 2);
        let bound_instance = FunctionInstanceId::from_index(base_instances);
        let drop_artifact = LoweredArtifactRequestId::from_index(base_artifacts + 1);
        let drop_ordinal = program
            .artifacts
            .get(drop_artifact)
            .expect("artifact")
            .ordinal;
        let record = program
            .artifacts
            .get(LoweredArtifactRequestId::from_index(base_artifacts))
            .expect("structural artifact");
        let Some(LoweredArtifactPlan::StructuralMethod(plan)) = &record.plan else {
            panic!("the structural artifact keeps a structural plan");
        };
        let StructuralBody::ProductDebug { steps, write } = &plan.body else {
            panic!("the expander kept the planned product-Debug body");
        };
        assert_eq!(write.instance, Some(bound_instance));
        assert_eq!(write.kind, LoweredInstanceDependencyKind::FormattingWrite);
        let DebugStep::Element { delegate, .. } = &steps[0] else {
            panic!("the planned step is an element");
        };
        let PlannedCallee::Artifact(drop) = &delegate.callee else {
            panic!("the planned delegate is an artifact");
        };
        assert_eq!(drop.artifact, Some(drop_ordinal));

        // The bound callees match the artifact-owned edges one-to-one.
        assert_eq!(record.instances.len(), 1);
        assert_eq!(record.instances[0].instance, bound_instance);
        assert_eq!(
            record.instances[0].kind,
            LoweredInstanceDependencyKind::FormattingWrite
        );
        assert_eq!(record.artifacts.len(), 1);
        assert_eq!(record.artifacts[0].artifact, drop_ordinal);
        assert_eq!(
            record.artifacts[0].kind,
            LoweredArtifactDependencyKind::DropGlue
        );
    }

    #[test]
    fn closure_plans_with_uninterned_callees_are_diagnosed() {
        let mut program = stage_three(IDENTITY_FIXTURE);
        let seed = identity_instance(&program, 0);
        let origin = instance_origin(&program, seed);
        let base_artifacts = program.artifacts.len();
        // A planned instance the expander never requests: nothing interns it.
        let missing_type = CheckedType::Ref(Box::new(CheckedType::U16));
        let resolved = resolve_root_instance(&program, "identity", missing_type, &origin);
        let write = PlannedInstance {
            key: resolved.key.clone(),
            instance: None,
            kind: LoweredInstanceDependencyKind::FormattingWrite,
        };
        let drop_type = CheckedType::Ref(Box::new(CheckedType::I32));
        let drop_key = ArtifactRequestKey::DropGlue(
            CanonicalType::concrete(&drop_type, &origin).expect("type"),
        );
        let delegate = PlannedCallee::Artifact(PlannedArtifact {
            key: drop_key,
            artifact: None,
            kind: LoweredArtifactDependencyKind::DropGlue,
        });
        let (key, plan) = planned_debug_plan(&program, seed, &origin, write, delegate);

        let mut hooks = TestHooks::default();
        hooks.instance_requests.insert(
            seed.index(),
            vec![vec![ClosureRequest::Artifact {
                key,
                plan,
                kind: LoweredArtifactDependencyKind::StructuralMethod,
                origin: origin.clone(),
                use_site: Some(ArtifactUseSite::Test(0)),
            }]],
        );
        // The artifact expands to nothing, so neither planned callee is
        // interned and the binding pass must diagnose both.
        let diagnostics = program.close_artifact_catalog(&hooks);
        let messages = messages(&diagnostics);
        assert!(
            messages
                .iter()
                .any(|message| message.contains("planned instance callee that was never interned")),
            "{messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|message| message.contains("planned artifact callee that was never interned")),
            "{messages:?}"
        );
        assert_eq!(program.artifacts.len(), base_artifacts + 1);
    }

    #[test]
    fn closure_edges_without_planned_callees_are_diagnosed() {
        let mut program = stage_three(IDENTITY_FIXTURE);
        let seed = identity_instance(&program, 0);
        let origin = instance_origin(&program, seed);
        let base_artifacts = program.artifacts.len();
        let callable_type = program
            .instances
            .get(seed)
            .and_then(|instance| instance.body.as_ref())
            .expect("seed body")
            .signature
            .clone();
        let key = ArtifactRequestKey::StructuralMethod(
            StructuralMethodKey::new(
                StructuralTraitMethod::Debug,
                TraitId(3),
                TraitMethodId(4),
                &[CheckedType::I32],
                &callable_type,
                &origin,
            )
            .expect("concrete key"),
        );
        // A complete plan that names no callee.
        let plan = LoweredArtifactPlan::StructuralMethod(StructuralMethodPlan {
            structural: StructuralTraitMethod::Debug,
            trait_id: TraitId(3),
            method: TraitMethodId(4),
            arguments: vec![CheckedType::I32],
            callable_type,
            body: StructuralBody::IndexLoad {
                element: CheckedType::I32,
                length: 1,
                output: CheckedType::I32,
            },
        });
        let drop_type = CheckedType::Ref(Box::new(CheckedType::I32));
        let drop_key = ArtifactRequestKey::DropGlue(
            CanonicalType::concrete(&drop_type, &origin).expect("type"),
        );

        let mut hooks = TestHooks::default();
        hooks.instance_requests.insert(
            seed.index(),
            vec![vec![ClosureRequest::Artifact {
                key,
                plan,
                kind: LoweredArtifactDependencyKind::StructuralMethod,
                origin: origin.clone(),
                use_site: Some(ArtifactUseSite::Test(0)),
            }]],
        );
        // The expander requests an artifact its plan does not name.
        hooks.artifact_requests.insert(
            base_artifacts,
            vec![vec![ClosureRequest::Artifact {
                key: drop_key,
                plan: LoweredArtifactPlan::DropGlue(DropGluePlan {
                    value_type: drop_type,
                    body: DropGlueBody::Unexpanded,
                }),
                kind: LoweredArtifactDependencyKind::DropGlue,
                origin: origin.clone(),
                use_site: None,
            }]],
        );
        assert!(program.close_artifact_catalog(&hooks).is_empty());
        let diagnostics = program.validate_artifact_closure(&hooks);
        assert!(
            messages(&diagnostics)
                .iter()
                .any(|message| message.contains("artifact-owned artifact edge")
                    && message.contains("with no planned callee")),
            "{diagnostics:?}"
        );
    }

    /// Hooks whose artifact re-expansion returns a different plan the second
    /// time, proving the fixed-point plan comparison detects nondeterminism.
    struct PlanChangingHooks {
        seed: FunctionInstanceId,
        origin: Origin,
        first: LoweredArtifactPlan,
        second: LoweredArtifactPlan,
        calls: Cell<usize>,
    }

    impl ArtifactFamilyHooks for PlanChangingHooks {
        fn scan_initializer(
            &self,
            _program: &LoweredProgram,
            _initializer: InitializerId,
        ) -> ScanResult {
            Ok(Vec::new())
        }

        fn scan_instance(
            &self,
            _program: &LoweredProgram,
            instance: FunctionInstanceId,
        ) -> ScanResult {
            if instance != self.seed {
                return Ok(Vec::new());
            }
            Ok(vec![ClosureRequest::Artifact {
                key: ArtifactRequestKey::DropGlue(
                    CanonicalType::concrete(&CheckedType::I32, &self.origin).expect("type"),
                ),
                plan: self.first.clone(),
                kind: LoweredArtifactDependencyKind::DropGlue,
                origin: self.origin.clone(),
                use_site: Some(ArtifactUseSite::Test(0)),
            }])
        }

        fn expand(
            &self,
            _program: &LoweredProgram,
            _artifact: LoweredArtifactRequestId,
        ) -> ExpansionResult {
            let call = self.calls.get();
            self.calls.set(call + 1);
            if call == 0 {
                Ok((self.first.clone(), Vec::new()))
            } else {
                Ok((self.second.clone(), Vec::new()))
            }
        }
    }

    #[test]
    fn closure_fixed_point_detects_a_changed_plan() {
        let mut program = stage_three(IDENTITY_FIXTURE);
        let seed = identity_instance(&program, 0);
        let origin = instance_origin(&program, seed);
        let first = LoweredArtifactPlan::DropGlue(DropGluePlan {
            value_type: CheckedType::I32,
            body: DropGlueBody::Unexpanded,
        });
        let second = LoweredArtifactPlan::DropGlue(DropGluePlan {
            value_type: CheckedType::I64,
            body: DropGlueBody::Unexpanded,
        });
        let hooks = PlanChangingHooks {
            seed,
            origin,
            first,
            second,
            calls: Cell::new(0),
        };
        assert!(program.close_artifact_catalog(&hooks).is_empty());
        let diagnostics = program.validate_artifact_closure(&hooks);
        assert!(
            messages(&diagnostics)
                .iter()
                .any(|message| message.contains("re-expanded to a different plan")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn closure_initializer_fixed_point_compares_edge_kinds() {
        let mut program = stage_three("let value = 1\n");
        let origin = program
            .initializers
            .get(InitializerId::from_index(0))
            .expect("initializer")
            .origin
            .clone();
        let value_type = CheckedType::Ref(Box::new(CheckedType::I32));
        let recorded =
            drop_glue_request(value_type.clone(), &origin, Some(ArtifactUseSite::Test(5)));
        // Same key and origin on the re-check, but a different edge kind.
        let ClosureRequest::Artifact {
            key,
            plan,
            origin: request_origin,
            use_site,
            ..
        } = recorded.clone()
        else {
            unreachable!("drop_glue_request builds an artifact request");
        };
        let changed_kind = ClosureRequest::Artifact {
            key,
            plan,
            kind: LoweredArtifactDependencyKind::GcFinalizer,
            origin: request_origin,
            use_site,
        };
        let mut hooks = TestHooks::default();
        hooks
            .initializer_requests
            .insert(0, vec![vec![recorded], vec![changed_kind]]);
        assert!(program.close_artifact_catalog(&hooks).is_empty());
        let diagnostics = program.validate_artifact_closure(&hooks);
        assert!(
            messages(&diagnostics).iter().any(|message| message
                .contains("closure did not reach a fixed point: initializer 0 has no edge")),
            "{diagnostics:?}"
        );
    }
}
