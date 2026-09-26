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
    ArenaId, FunctionInstanceId, InitializerId, LoweredArtifactDependency,
    LoweredArtifactDependencyKind, LoweredArtifactPlan, LoweredArtifactRequestId,
    LoweredArtifactRequestRoot, LoweredInstanceDependencyKind, LoweredInstanceRequest,
    LoweredProgram, Origin, ResolvedInstanceRequest,
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
pub(super) enum ClosureRequest {
    /// A source-function instance, already resolved through
    /// `LoweredProgram::resolve_instance_request` by the requester with a
    /// fully concrete recipe. The engine never guesses substitutions.
    Instance {
        resolved: ResolvedInstanceRequest,
        kind: LoweredInstanceDependencyKind,
        origin: Origin,
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
/// family variants; Stages 4.4 and 4.5 add drop facts, allocations, closure
/// constructions, reactive operations, `coro` creations, and intrinsic calls.
/// Every match on this enum must stay exhaustive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ArtifactUseSite {
    /// A scripted test site identified by its position in the hook table.
    #[cfg(test)]
    Test(u32),
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
}

/// The production hook set. Stage 4.2 scanners request nothing and expanders
/// keep the existing Stage 4.1 placeholder plan, so the loop is a no-op over
/// the Stage 3 catalog. Stages 4.3-4.6 replace the family arms.
pub(super) struct ProductionHooks;

impl ArtifactFamilyHooks for ProductionHooks {
    fn scan_initializer(
        &self,
        _program: &LoweredProgram,
        _initializer: InitializerId,
    ) -> ScanResult {
        // Family scanners are composed in the fixed order 4.3 -> 4.4 -> 4.5 ->
        // 4.6. Stage 4.2 registers none.
        Ok(Vec::new())
    }

    fn scan_instance(
        &self,
        _program: &LoweredProgram,
        _instance: FunctionInstanceId,
    ) -> ScanResult {
        Ok(Vec::new())
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
        // Every family keeps its placeholder plan in Stage 4.2. The match is
        // exhaustive so a new key family is never silently ignored; each later
        // substage replaces its own arm.
        match key {
            ArtifactRequestKey::ConstructorAdapter(_)
            | ArtifactRequestKey::StructuralMethod(_)
            | ArtifactRequestKey::DropGlue(_)
            | ArtifactRequestKey::GcFinalizer(_)
            | ArtifactRequestKey::CoroutineCodes(_)
            | ArtifactRequestKey::ReactionRunner(_)
            | ArtifactRequestKey::UntilRunner(_)
            | ArtifactRequestKey::DerivedRunner(_)
            | ArtifactRequestKey::ExternAdapter(_) => Ok((plan, Vec::new())),
        }
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

        let baseline_instances = self.instances.len();
        let baseline_artifacts = self.artifacts.len();
        let growth_budget = self
            .functions
            .iter()
            .count()
            .saturating_mul(GROWTH_PER_TEMPLATE)
            .max(MIN_GROWTH_BUDGET);

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
                let mut diagnostics = self.apply_closure_requests(
                    AppliedOwner::Initializer(initializer),
                    requests,
                    &mut new_instances,
                    &mut last_request,
                );
                if !diagnostics.is_empty() {
                    diagnostics.shrink_to_fit();
                    return diagnostics;
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
            }

            // Expand artifacts to the current end. Expanding one artifact may
            // append more, and the cursor keeps going until it reaches the
            // end, so every artifact is expanded exactly once per closure.
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
                }
            }

            if new_instances.is_empty() {
                return self.finish_closure();
            }

            let growth = (self.instances.len() - baseline_instances)
                + (self.artifacts.len() - baseline_artifacts);
            if growth > growth_budget {
                return vec![self.non_convergence(
                    &last_request,
                    format!("artifact closure grew by {growth} entries (budget {growth_budget})"),
                )];
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

    /// Re-assigns catalog names over the final graph and returns any name
    /// collision diagnostics. Names are stable for a fixed catalog, so running
    /// this at the end of the closure (and again at the end of every resume)
    /// never renames an earlier entry.
    fn finish_closure(&mut self) -> Vec<Diagnostic> {
        let mut recorder = GraphRecorder::from_parts(self.take_graph());
        let result = recorder.assign_names();
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
                } => {
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
                    if matches!(owner, AppliedOwner::Initializer(_)) {
                        // Stage 3 initializer instance requests keep their
                        // existing request-root-only representation.
                    } else {
                        recorder.record_instance_edge(
                            owner.traversal_owner(),
                            instance,
                            &origin,
                            kind,
                        );
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
                    let (ordinal, _) = recorder.request_artifact(
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
