//! Stage 4.6: the per-program lowered runtime requirements.
//!
//! Fixed-named runtime symbols have no lowered bodies to plan, so they are not
//! an artifact family. Instead every program carries an ordered, deduplicated
//! set of the runtime surfaces its lowered operations need. The set is derived
//! from the closed catalog: materialized instance bodies (including the
//! eagerly materialized standard-library templates the backend always emits),
//! module initializers, and expanded artifact plans. Stage 5 may keep
//! installing the surfaces by name, but only when the requirement is present
//! and with the same eager-root behavior.
//!
//! The set records surfaces, not individual symbols, because a subsystem's
//! symbols are installed together: the garbage-collector module, the
//! coroutine/scheduler/completion module, and the reactive module, plus the
//! UTF-8 validator and the lazily declared libc symbols. `llvm.trap` stays
//! backend-local: it is a pure LLVM intrinsic with no runtime installation and
//! no lowered body, as the Stage 4.1 negative matrix records.

use staple_syntax::Diagnostic;

use super::cleanup_artifacts::{LoweredOwnerVisitor, OwnerArenas, walk_owner};
use super::{
    ConstructorConstruction, DropGlueBody, IntrinsicFunction, LoweredArtifactPlan,
    LoweredAwaitKind, LoweredBindingItem, LoweredCall, LoweredCallableTarget, LoweredCallableValue,
    LoweredCallableValueId, LoweredClosureEnvironment, LoweredPattern, LoweredPatternKind,
    LoweredProgram, LoweredStringTemplate, LoweredStringTemplatePart, Origin, RuntimeRelease,
    StructuralBody,
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
    /// libc `free` for C-string cleanup.
    CStringFree,
    /// libc `memcmp` for string-literal pattern comparison.
    LiteralComparison,
    /// libc `snprintf` for numeric-to-string conversion.
    NumericToString,
    /// libc `strlen` for C-string conversion.
    CStringLength,
    /// libc `memchr` for interior-NUL checks.
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
            .expect("every requirement is listed in ALL")
    }

    /// The requirement a fixed runtime symbol belongs to, when the symbol is
    /// one of the recorded surfaces. Used by the legacy transition comparison.
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

    pub(crate) fn requirements(&self) -> &[RuntimeRequirement] {
        &self.requirements
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.requirements.is_empty()
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
        let derived = self.derive_runtime_requirements();
        self.runtime_requirements = derived;
        Vec::new()
    }

    /// The read-only derivation used by recording and by the fixed-point
    /// validator. Never mutates the program.
    pub(super) fn derive_runtime_requirements(&self) -> LoweredRuntimeRequirements {
        let mut requirements = LoweredRuntimeRequirements::default();
        let mut diagnostics = Vec::new();
        for (id, _) in self.initializers.iter() {
            let mut visitor = RequirementVisitor {
                program: self,
                requirements: &mut requirements,
            };
            if let Err(mut problems) = walk_owner(self, OwnerArenas::Initializer(id), &mut visitor)
            {
                diagnostics.append(&mut problems);
            }
        }
        for (_, instance) in self.instances.iter() {
            let Some(body) = instance.body.as_ref() else {
                continue;
            };
            let mut visitor = RequirementVisitor {
                program: self,
                requirements: &mut requirements,
            };
            if let Err(mut problems) = walk_owner(self, OwnerArenas::Instance(body), &mut visitor) {
                diagnostics.append(&mut problems);
            }
        }
        for (_, artifact) in self.artifacts.iter() {
            if let Some(plan) = &artifact.plan {
                record_plan_requirements(plan, &mut requirements);
            }
        }
        debug_assert!(
            diagnostics.is_empty(),
            "runtime requirement derivation failed: {diagnostics:?}"
        );
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
                IntrinsicFunction::BufferWithCapacity | IntrinsicFunction::BufferClone => self.gc(),
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
            && let Some(record) = self.program.symbols.get(symbol)
            && record.captured_cell
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
        if matches!(pattern.kind, LoweredPatternKind::Literal { .. }) {
            self.requirements
                .record(RuntimeRequirement::LiteralComparison);
        }
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
    let derived = program.derive_runtime_requirements();
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
