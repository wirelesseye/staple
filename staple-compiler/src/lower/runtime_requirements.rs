//! Ordered runtime requirements of a closed lowered program.
//!
//! Fixed-name runtime surfaces are separate from generated artifacts. Materialized
//! instances, initializers, and expanded plans contribute to a deduplicated set.
//! Codegen links only recorded surfaces: GC, coroutine/scheduler/completion,
//! reactive execution, UTF-8 validation, and lazily declared libc functions.
//! LLVM intrinsics such as llvm.trap need no runtime module or lowered body.

use staple_syntax::Diagnostic;

use super::cleanup_artifacts::{LoweredOwnerVisitor, walk_owner};
use super::emission::OwnerArenas;
use super::{
    ConstructorConstruction, DropGlueBody, IntrinsicFunction, LoweredArtifactPlan,
    LoweredAwaitKind, LoweredBindingItem, LoweredCall, LoweredCallableTarget, LoweredCallableValue,
    LoweredCallableValueId, LoweredClosureEnvironment, LoweredPattern, LoweredPatternKind,
    LoweredProgram, LoweredStringTemplate, LoweredStringTemplatePart, Origin, RuntimeRelease,
    StructuralBody, SymbolId, SymbolStorage,
};

/// One runtime surface a lowered program needs. The declaration order is the
/// canonical recording order, so repeated lowering yields byte-identical
/// requirement lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RuntimeRequirement {
    /// The garbage-collector runtime module (`gc.ll`).
    GarbageCollector,
    /// The coroutine, scheduler, task, and completion runtime module
    /// (`coroutine.ll`).
    CoroutineRuntime,
    /// The reactive runtime module (`reactive.ll`).
    ReactiveRuntime,
    /// The UTF-8 validator (`__staple_is_valid_utf8`).
    Utf8Validator,
    /// Libc `free` for C-string cleanup.
    CStringFree,
    /// Libc `memcmp` for string-literal pattern comparison.
    LiteralComparison,
    /// Libc `snprintf` for numeric-to-string conversion.
    NumericToString,
    /// Libc `strlen` for C-string conversion.
    CStringLength,
    /// Libc `memchr` for interior-NUL checks.
    InteriorNulCheck,
}

impl RuntimeRequirement {
    /// Every requirement in canonical order.
    pub(crate) const ALL: [RuntimeRequirement; 9] = [
        RuntimeRequirement::GarbageCollector,
        RuntimeRequirement::CoroutineRuntime,
        RuntimeRequirement::ReactiveRuntime,
        RuntimeRequirement::Utf8Validator,
        RuntimeRequirement::CStringFree,
        RuntimeRequirement::LiteralComparison,
        RuntimeRequirement::NumericToString,
        RuntimeRequirement::CStringLength,
        RuntimeRequirement::InteriorNulCheck,
    ];

    /// Stable description for diagnostics and snapshots.
    pub(crate) fn description(self) -> &'static str {
        match self {
            RuntimeRequirement::GarbageCollector => "garbage-collector",
            RuntimeRequirement::CoroutineRuntime => "coroutine-runtime",
            RuntimeRequirement::ReactiveRuntime => "reactive-runtime",
            RuntimeRequirement::Utf8Validator => "utf8-validator",
            RuntimeRequirement::CStringFree => "c-string-free",
            RuntimeRequirement::LiteralComparison => "literal-comparison",
            RuntimeRequirement::NumericToString => "numeric-to-string",
            RuntimeRequirement::CStringLength => "c-string-length",
            RuntimeRequirement::InteriorNulCheck => "interior-nul-check",
        }
    }

    fn position(self) -> usize {
        RuntimeRequirement::ALL
            .iter()
            .position(|candidate| *candidate == self)
            .expect("internal invariant violated: every requirement is listed in ALL")
    }

    #[cfg(test)]
    /// The requirement a fixed runtime symbol belongs to, when the symbol is
    /// one of the recorded surfaces. Used by the runtime requirement tests.
    pub(crate) fn for_runtime_symbol(name: &str) -> Option<RuntimeRequirement> {
        if name.starts_with("__staple_gc_") {
            return Some(RuntimeRequirement::GarbageCollector);
        }
        if name.starts_with("__staple_coro_")
            || name.starts_with("__staple_sched_")
            || name.starts_with("__staple_task_")
            || name.starts_with("__staple_completion_")
        {
            return Some(RuntimeRequirement::CoroutineRuntime);
        }
        if name.starts_with("__staple_reactive_")
            || name.starts_with("__staple_reaction_")
            || name.starts_with("__staple_batch_")
            || name.starts_with("__staple_until_")
            || name.starts_with("__staple_derived_")
            || name.starts_with("__staple_signal_")
            || name.starts_with("__staple_tracking_")
        {
            return Some(RuntimeRequirement::ReactiveRuntime);
        }
        match name {
            "__staple_is_valid_utf8" => Some(RuntimeRequirement::Utf8Validator),
            "free" => Some(RuntimeRequirement::CStringFree),
            "memcmp" => Some(RuntimeRequirement::LiteralComparison),
            "snprintf" => Some(RuntimeRequirement::NumericToString),
            "strlen" => Some(RuntimeRequirement::CStringLength),
            "memchr" => Some(RuntimeRequirement::InteriorNulCheck),
            _ => None,
        }
    }
}

/// An ordered, deduplicated set of runtime surfaces. Recording preserves the
/// canonical `RuntimeRequirement::ALL` order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LoweredRuntimeRequirements {
    requirements: Vec<RuntimeRequirement>,
}

impl LoweredRuntimeRequirements {
    pub(crate) fn record(&mut self, requirement: RuntimeRequirement) {
        if !self.requirements.contains(&requirement) {
            self.requirements.push(requirement);
        }
    }

    /// Sorts into canonical order; every recording pass ends with this so the
    /// stored set is order-independent and deterministic.
    fn canonicalized(mut self) -> Self {
        self.requirements
            .sort_by_key(|requirement| requirement.position());
        self
    }

    pub(crate) fn contains(&self, requirement: RuntimeRequirement) -> bool {
        self.requirements.contains(&requirement)
    }

    #[cfg(test)]
    pub(crate) fn requirements(&self) -> &[RuntimeRequirement] {
        &self.requirements
    }

    /// The requirements the two sets disagree on, as `(extra, missing)`.
    pub(crate) fn difference(
        &self,
        other: &LoweredRuntimeRequirements,
    ) -> (Vec<RuntimeRequirement>, Vec<RuntimeRequirement>) {
        let extra = self
            .requirements
            .iter()
            .filter(|requirement| !other.contains(**requirement))
            .copied()
            .collect();
        let missing = other
            .requirements
            .iter()
            .filter(|requirement| !self.contains(**requirement))
            .copied()
            .collect();
        (extra, missing)
    }
}

impl LoweredProgram {
    /// Records the runtime surfaces the closed catalog needs. Runs after the
    /// closure fixed point so expanded plans are visible, and rebuilds the set
    /// from scratch so repeated lowering is stable.
    pub(super) fn record_runtime_requirements(&mut self) -> Vec<Diagnostic> {
        let (derived, diagnostics) = self.derive_runtime_requirements();
        self.runtime_requirements = derived;
        diagnostics
    }

    /// The read-only derivation used by recording and by the fixed-point
    /// validator. Never mutates the program. A walk that fails leaves its
    /// owner's surfaces incomplete, so its diagnostics are returned rather
    /// than dropped.
    pub(super) fn derive_runtime_requirements(
        &self,
    ) -> (LoweredRuntimeRequirements, Vec<Diagnostic>) {
        let mut requirements = LoweredRuntimeRequirements::default();
        let mut diagnostics = Vec::new();
        for (id, _) in self.initializers.iter() {
            if let Err(mut problems) =
                self.record_owner_requirements(OwnerArenas::Initializer(id), &mut requirements)
            {
                diagnostics.append(&mut problems);
            }
        }
        for (_, instance) in self.instances.iter() {
            let Some(body) = instance.body.as_ref() else {
                continue;
            };
            if let Err(mut problems) =
                self.record_owner_requirements(OwnerArenas::Instance(body), &mut requirements)
            {
                diagnostics.append(&mut problems);
            }
        }
        for (_, artifact) in self.artifacts.iter() {
            if let Some(plan) = &artifact.plan {
                record_plan_requirements(plan, &mut requirements);
            }
        }
        (requirements.canonicalized(), diagnostics)
    }

    /// Records the surfaces one owner's lowered body needs.
    fn record_owner_requirements(
        &self,
        owner: OwnerArenas<'_>,
        requirements: &mut LoweredRuntimeRequirements,
    ) -> Result<(), Vec<Diagnostic>> {
        let mut visitor = RequirementVisitor {
            program: self,
            owner,
            requirements,
        };
        walk_owner(self, owner, &mut visitor)
    }

    /// Test-only: the surfaces one owner's emitted function references, for
    /// per-function comparison where the program-wide set is masked by the
    /// eagerly emitted standard library. The emitter inlines drop glue at each drop
    /// site (nested product, sum, and wrapper glue included), so the owner's
    /// drop-glue uses contribute their releases; finalizers and user `Drop`
    /// methods are separate functions and do not.
    #[cfg(test)]
    pub(super) fn owner_runtime_requirements(
        &self,
        owner: OwnerArenas<'_>,
        uses: &[super::LoweredArtifactUse],
    ) -> LoweredRuntimeRequirements {
        let mut requirements = LoweredRuntimeRequirements::default();
        self.record_owner_requirements(owner, &mut requirements)
            .expect("the owner walks cleanly");
        let mut pending = uses.iter().map(|use_| use_.artifact).collect::<Vec<_>>();
        let mut seen = std::collections::HashSet::new();
        while let Some(ordinal) = pending.pop() {
            if !seen.insert(ordinal) {
                continue;
            }
            let Some(LoweredArtifactPlan::DropGlue(plan)) = self
                .artifacts
                .iter()
                .find(|(_, artifact)| artifact.ordinal == ordinal)
                .and_then(|(_, artifact)| artifact.plan.as_ref())
            else {
                continue;
            };
            record_plan_requirements(
                &LoweredArtifactPlan::DropGlue(plan.clone()),
                &mut requirements,
            );
            let nested = match &plan.body {
                DropGlueBody::Product { fields } => fields
                    .iter()
                    .filter_map(|field| field.glue.artifact)
                    .collect(),
                DropGlueBody::Sum { alternatives } => alternatives
                    .iter()
                    .filter_map(|alternative| alternative.glue.artifact)
                    .collect(),
                DropGlueBody::Wrapper { representation } => {
                    representation.artifact.into_iter().collect()
                }
                DropGlueBody::UserDrop { representation, .. } => representation
                    .as_ref()
                    .and_then(|glue| glue.artifact)
                    .into_iter()
                    .collect(),
                _ => Vec::new(),
            };
            pending.extend(nested);
        }
        requirements.canonicalized()
    }
}

/// Records the surfaces one expanded artifact plan implies.
fn record_plan_requirements(
    plan: &LoweredArtifactPlan,
    requirements: &mut LoweredRuntimeRequirements,
) {
    match plan {
        LoweredArtifactPlan::ConstructorAdapter(plan) => {
            if matches!(
                plan.construction,
                ConstructorConstruction::ManagedRef { .. }
            ) {
                requirements.record(RuntimeRequirement::GarbageCollector);
            }
        }
        LoweredArtifactPlan::StructuralMethod(plan) => {
            if let StructuralBody::ProductDebug { steps, .. } = &plan.body
                && steps
                    .iter()
                    .any(|step| matches!(step, super::DebugStep::Write(_)))
            {
                // Literal Debug text allocates its string data through the GC.
                requirements.record(RuntimeRequirement::GarbageCollector);
            }
        }
        LoweredArtifactPlan::DropGlue(plan) => match &plan.body {
            DropGlueBody::RuntimeRelease(RuntimeRelease::SchedulerDestroy) => {
                requirements.record(RuntimeRequirement::CoroutineRuntime);
            }
            DropGlueBody::RuntimeRelease(
                RuntimeRelease::WaitDrop
                | RuntimeRelease::ResolverDrop
                | RuntimeRelease::CompletionTokenRelease,
            ) => {
                requirements.record(RuntimeRequirement::CoroutineRuntime);
            }
            DropGlueBody::CStringFree => {
                requirements.record(RuntimeRequirement::CStringFree);
            }
            _ => {}
        },
        LoweredArtifactPlan::GcFinalizer(_) => {
            requirements.record(RuntimeRequirement::GarbageCollector);
        }
        LoweredArtifactPlan::CoroutineCodes(plan) => {
            requirements.record(RuntimeRequirement::CoroutineRuntime);
            if plan.frame.is_some() {
                requirements.record(RuntimeRequirement::GarbageCollector);
            }
        }
        LoweredArtifactPlan::ReactionRunner(plan) => {
            if !matches!(plan.body, super::ReactiveRunnerBody::Unexpanded) {
                requirements.record(RuntimeRequirement::ReactiveRuntime);
            }
        }
        LoweredArtifactPlan::UntilRunner(plan) => {
            if !matches!(plan.body, super::ReactiveRunnerBody::Unexpanded) {
                requirements.record(RuntimeRequirement::ReactiveRuntime);
                requirements.record(RuntimeRequirement::CoroutineRuntime);
            }
        }
        LoweredArtifactPlan::DerivedRunner(plan) => {
            if !matches!(plan.body, super::ReactiveRunnerBody::Unexpanded) {
                requirements.record(RuntimeRequirement::ReactiveRuntime);
                requirements.record(RuntimeRequirement::GarbageCollector);
            }
        }
        LoweredArtifactPlan::ExternAdapter(_) => {}
    }
}

/// The body-walking visitor that records runtime requirements. It reads the
/// same lowered facts the backend consumes, never emission.
struct RequirementVisitor<'a> {
    program: &'a LoweredProgram,
    owner: OwnerArenas<'a>,
    requirements: &'a mut LoweredRuntimeRequirements,
}

impl RequirementVisitor<'_> {
    fn gc(&mut self) {
        self.requirements
            .record(RuntimeRequirement::GarbageCollector);
    }

    fn coroutine(&mut self) {
        self.requirements
            .record(RuntimeRequirement::CoroutineRuntime);
    }

    fn reactive(&mut self) {
        self.requirements
            .record(RuntimeRequirement::ReactiveRuntime);
    }

    /// Whether binding `symbol` GC-allocates its cell. The emitter
    /// `allocate_binding_cell` allocates through the collector exactly when
    /// the symbol is in `captured_cell_symbols`: some function captures it and
    /// it has mutable storage or is derived. `LoweredSymbol::captured_cell` is
    /// not that fact (it says whether a capture *would* need a cell, captured
    /// or not). Module globals live in global storage, a mutated parameter
    /// arrives as the caller's pointer, and a coroutine frame binding already
    /// has its frame cell, so none of them allocate one.
    fn allocates_captured_cell(&self, symbol: SymbolId) -> bool {
        self.program.symbols.get(symbol).is_some_and(|record| {
            record.captured
                && (record.mutable_storage || record.derived)
                && record.storage != SymbolStorage::GlobalStorage
                && !record.mutated_parameter
        }) && !self.is_frame_binding(symbol)
    }

    /// Whether `symbol` is a frame binding of the coroutine body this owner
    /// is; the emitter pre-seeds those as frame cells before any block predeclares.
    fn is_frame_binding(&self, symbol: SymbolId) -> bool {
        let OwnerArenas::Instance(body) = self.owner else {
            return false;
        };
        body.plan_template
            .and_then(|plan| body.plan(plan))
            .is_some_and(|plan| plan.frame_bindings.contains(&symbol))
    }
}

impl LoweredOwnerVisitor for RequirementVisitor<'_> {
    fn call_site(&mut self, call: &LoweredCall) -> Result<(), Vec<Diagnostic>> {
        match &call.target {
            LoweredCallableTarget::Constructor {
                recursive: Some(_), ..
            } => self.gc(),
            LoweredCallableTarget::Intrinsic { intrinsic, .. } => match intrinsic {
                IntrinsicFunction::ToString { .. } => {
                    self.requirements
                        .record(RuntimeRequirement::NumericToString);
                    self.gc();
                }
                IntrinsicFunction::StringFromCString => {
                    self.requirements.record(RuntimeRequirement::CStringLength);
                    self.requirements.record(RuntimeRequirement::Utf8Validator);
                    self.requirements.record(RuntimeRequirement::CStringFree);
                    self.gc();
                }
                IntrinsicFunction::StringToCString => {
                    self.requirements
                        .record(RuntimeRequirement::InteriorNulCheck);
                }
                IntrinsicFunction::StringAdd => self.gc(),
                IntrinsicFunction::BufferWithCapacity
                | IntrinsicFunction::BufferClone
                | IntrinsicFunction::BufferGet
                | IntrinsicFunction::BufferFreeze => self.gc(),
                IntrinsicFunction::CoroutineBlockOn
                | IntrinsicFunction::SchedulerCreate
                | IntrinsicFunction::TaskScope
                | IntrinsicFunction::Spawn
                | IntrinsicFunction::Pump
                | IntrinsicFunction::YieldNow
                | IntrinsicFunction::TaskIsFinished
                | IntrinsicFunction::TaskCancel
                | IntrinsicFunction::Completion
                | IntrinsicFunction::CompletionWithCancel
                | IntrinsicFunction::CompletionToken
                | IntrinsicFunction::CompletionTokenResolve
                | IntrinsicFunction::CompletionTokenCancel
                | IntrinsicFunction::ResolverComplete
                | IntrinsicFunction::ResolverCancel => {
                    self.coroutine();
                    self.gc();
                }
                IntrinsicFunction::ReactiveScope
                | IntrinsicFunction::Reaction
                | IntrinsicFunction::Batch
                | IntrinsicFunction::Snapshot
                | IntrinsicFunction::Until => self.reactive(),
                _ => {}
            },
            _ => {}
        }
        Ok(())
    }

    fn callable_value_site(
        &mut self,
        _id: LoweredCallableValueId,
        value: &LoweredCallableValue,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        if let Some(closure) = &value.closure
            && closure.environment == LoweredClosureEnvironment::Fresh
            && !closure.captures.is_empty()
        {
            self.gc();
        }
        Ok(())
    }

    fn binding_site(
        &mut self,
        binding: &LoweredBindingItem,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        if binding.signal {
            self.reactive();
        }
        if let Some(symbol) = binding.symbol
            && self.allocates_captured_cell(symbol)
        {
            self.gc();
        }
        // `predeclare_checked_bindings`: a block-local `def` that an earlier
        // closure reads before its initializer gets a malloc'd state cell
        // registered as a GC root region. Module globals keep their state in
        // global storage, and a coroutine frame binding already owns a frame
        // cell, so neither registers one.
        if binding.requires_initialization_check
            && let Some(symbol) = binding.symbol
            && self
                .program
                .symbols
                .get(symbol)
                .is_some_and(|record| record.storage != SymbolStorage::GlobalStorage)
            && !self.is_frame_binding(symbol)
        {
            self.gc();
        }
        Ok(())
    }

    fn pattern_site(
        &mut self,
        pattern: &LoweredPattern,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        match pattern.kind {
            LoweredPatternKind::Literal { .. } => {
                self.requirements
                    .record(RuntimeRequirement::LiteralComparison);
            }
            LoweredPatternKind::Binding {
                symbol: Some(symbol),
                ..
            } if self.allocates_captured_cell(symbol) => self.gc(),
            _ => {}
        }
        Ok(())
    }

    fn string_literal_site(&mut self, _origin: &Origin) -> Result<(), Vec<Diagnostic>> {
        // The emitter copies every string literal's bytes into GC-allocated data.
        self.gc();
        Ok(())
    }

    fn string_template_site(
        &mut self,
        template: &LoweredStringTemplate,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        if template
            .parts
            .iter()
            .any(|part| matches!(part, LoweredStringTemplatePart::Literal(_)))
        {
            self.gc();
        }
        Ok(())
    }

    fn await_site(
        &mut self,
        await_: &super::LoweredAwait,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        self.coroutine();
        if matches!(
            await_.kind,
            LoweredAwaitKind::ChildCoroutine { until: true, .. }
        ) {
            self.reactive();
        }
        Ok(())
    }

    fn reactive_operation(
        &mut self,
        _id: super::LoweredReactiveOperationId,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        self.reactive();
        Ok(())
    }

    fn coro_creation(
        &mut self,
        _id: super::LoweredCoroId,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        self.coroutine();
        self.gc();
        Ok(())
    }
}

/// The fixed-point check: the stored requirement set must equal a fresh
/// derivation from the same catalog, so no requirement is stale and none is
/// missing.
pub(super) fn check_runtime_requirements(
    program: &LoweredProgram,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let (derived, mut problems) = program.derive_runtime_requirements();
    if !problems.is_empty() {
        diagnostics.append(&mut problems);
        return;
    }
    let stored = &program.runtime_requirements;
    if stored == &derived {
        return;
    }
    let (extra, missing) = stored.difference(&derived);
    for requirement in extra {
        diagnostics.push(Diagnostic::new(
            staple_syntax::Span::Compiler,
            format!(
                "runtime requirement `{}` has no lowered operation that needs it",
                requirement.description()
            ),
        ));
    }
    for requirement in missing {
        diagnostics.push(Diagnostic::new(
            staple_syntax::Span::Compiler,
            format!(
                "runtime requirement `{}` is missing from the recorded set",
                requirement.description()
            ),
        ));
    }
}
