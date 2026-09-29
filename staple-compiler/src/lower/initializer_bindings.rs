//! Stage 5.1 (D4): module-initializer dispatch bindings.
//!
//! Module initializers own program-arena bodies, not instance-local bodies, so
//! they never went through the Stage 3.4 site binder: their dispatch sites were
//! recorded as request roots and closure edges, and the Stage 4.3-4.6 scanners
//! re-resolved each one with the Stage 3.3 recipe. The Stage 5 emitter must
//! instead read a binding table exactly as it does for instance bodies, because
//! it must never resolve a dispatch site at LLVM time.
//!
//! This module builds that table at the closure fixed point: each initializer
//! is walked with the shared family-neutral `LoweredWalker`, and every
//! call/callable/thunk/trait/constructor/formatting/coroutine/reactive site is
//! resolved with the same recipe the scanners use (root target, empty enclosing
//! environment). The validator then applies the instance-table rules: every
//! dispatch site is bound with the expected shape, every binding names a live
//! site and a catalog entry, trait evidence exists exactly for trait-dependent
//! sites, each closure-phase use names the target the binding does, and every
//! instance first requested by this initializer is bound at some site.

use std::collections::{BTreeMap, HashSet};

use staple_syntax::{Diagnostic, Span};

use crate::specialization::{ArtifactRequestKey, ConstructorAdapterKey, StructuralMethodKey};
use crate::{CheckedFunctionType, FunctionId, SymbolId, TraitId, TraitMethodId, TypeId};

use super::cleanup_artifacts::{LoweredOwnerVisitor, walk_owner};
use super::emission::OwnerArenas;
use super::instance_body::{LoweredBindingSite, LoweredBoundTarget};
use super::instance_resolution::{InstanceResolutionRequest, InstanceResolutionTarget};
use super::worklist::{
    concretize_function_type, instantiate_method_type, trait_site_substitutions,
};
use super::{
    ArenaId, ArtifactUseSite, CallSubstitutions, ExpressionId, FunctionInstanceId, InitializerId,
    ItemId, LoweredArtifactRequestId, LoweredAwait, LoweredAwaitId, LoweredCall, LoweredCallId,
    LoweredCallableTarget, LoweredCallableValue, LoweredCallableValueId, LoweredCoroId,
    LoweredExpression, LoweredExpressionKind, LoweredItem, LoweredItemKind, LoweredProgram,
    LoweredReactiveOperationId, LoweredReactiveOperationKind, LoweredStringTemplate,
    LoweredStringTemplatePart, Origin, TraitEvidence,
};

impl LoweredProgram {
    /// Builds the Stage 5.1 (D4) initializer binding and evidence tables over
    /// the closed catalog. One table per initializer, indexed by
    /// `InitializerId`; instance bodies already carry the same tables.
    pub(super) fn bind_initializer_sites(&mut self) -> Vec<Diagnostic> {
        let count = self.initializers.len();
        let mut bindings = Vec::with_capacity(count);
        let mut evidence = Vec::with_capacity(count);
        let mut diagnostics = Vec::new();
        for index in 0..count {
            let initializer = InitializerId::from_index(index);
            let mut binder = InitializerBinder::new(self, initializer);
            if let Err(mut problems) =
                walk_owner(self, OwnerArenas::Initializer(initializer), &mut binder)
            {
                diagnostics.append(&mut problems);
            }
            diagnostics.append(&mut binder.diagnostics);
            bindings.push(std::mem::take(&mut binder.bindings));
            evidence.push(std::mem::take(&mut binder.evidence));
        }
        self.initializer_bindings = bindings;
        self.initializer_evidence = evidence;
        diagnostics
    }

    /// Validates the initializer binding tables with the instance-table rules.
    pub(super) fn validate_initializer_bindings(&self) -> Vec<Diagnostic> {
        let count = self.initializers.len();
        if self.initializer_bindings.len() != count || self.initializer_evidence.len() != count {
            return vec![Diagnostic::new(
                Span::Compiler,
                format!(
                    "initializer binding tables cover {} and {} owners for {count} initializers",
                    self.initializer_bindings.len(),
                    self.initializer_evidence.len()
                ),
            )];
        }
        let mut diagnostics = Vec::new();
        for index in 0..count {
            let initializer = InitializerId::from_index(index);
            let validator = InitializerBindingValidator {
                program: self,
                initializer,
                bindings: &self.initializer_bindings[index],
                evidence: &self.initializer_evidence[index],
                visited: HashSet::new(),
                diagnostics: Vec::new(),
            };
            diagnostics.append(&mut validator.run());
            diagnostics.append(&mut self.check_initializer_binding_fixed_point(initializer));
        }
        diagnostics
    }

    /// Re-resolves every site of one initializer from scratch and requires the
    /// stored tables to equal the fresh ones. The structural checks above
    /// prove each binding is well-formed; this proves it is the right one (a
    /// site bound to another existing instance passes every structural check).
    fn check_initializer_binding_fixed_point(&self, initializer: InitializerId) -> Vec<Diagnostic> {
        let index = initializer.index();
        let mut binder = InitializerBinder::new(self, initializer);
        let mut diagnostics = Vec::new();
        if let Err(mut problems) =
            walk_owner(self, OwnerArenas::Initializer(initializer), &mut binder)
        {
            diagnostics.append(&mut problems);
        }
        diagnostics.append(&mut binder.diagnostics);
        if !diagnostics.is_empty() {
            return diagnostics;
        }
        let span = self.initializer_span(initializer);
        compare_tables(
            &self.initializer_bindings[index],
            &binder.bindings,
            |site, stored, fresh| {
                diagnostics.push(Diagnostic::new(
                    span.clone(),
                    format!(
                        "initializer {index} site {site:?} binds {stored:?} but re-resolves to {fresh:?}"
                    ),
                ));
            },
        );
        compare_tables(
            &self.initializer_evidence[index],
            &binder.evidence,
            |site, stored, fresh| {
                diagnostics.push(Diagnostic::new(
                    span.clone(),
                    format!(
                        "initializer {index} site {site:?} carries evidence {stored:?} but re-resolves to {fresh:?}"
                    ),
                ));
            },
        );
        diagnostics
    }

    fn initializer_span(&self, initializer: InitializerId) -> Span {
        self.initializers
            .get(initializer)
            .map(|initializer| initializer.origin.span.clone())
            .unwrap_or(Span::Compiler)
    }
}

/// The site binder for one module initializer. It reads the program's
/// template arenas and records one target per dispatch/construction site; it
/// never mutates a shared arena node.
struct InitializerBinder<'a> {
    program: &'a LoweredProgram,
    initializer: InitializerId,
    bindings: BTreeMap<LoweredBindingSite, LoweredBoundTarget>,
    evidence: BTreeMap<LoweredBindingSite, TraitEvidence>,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> InitializerBinder<'a> {
    fn new(program: &'a LoweredProgram, initializer: InitializerId) -> Self {
        InitializerBinder {
            program,
            initializer,
            bindings: BTreeMap::new(),
            evidence: BTreeMap::new(),
            diagnostics: Vec::new(),
        }
    }

    fn report(&mut self, origin: &Origin, message: impl Into<String>) {
        self.diagnostics
            .push(Diagnostic::new(origin.span.clone(), message.into()));
    }

    /// Resolves one source-function target with the Stage 3.3 recipe, exactly
    /// as the initializer root traversal did (`Root` target, no enclosing
    /// environment), and records the interned instance.
    fn bind_instance(
        &mut self,
        site: LoweredBindingSite,
        origin: &Origin,
        function: FunctionId,
        function_type: CheckedFunctionType,
        substitutions: CallSubstitutions,
        evidence: Option<TraitEvidence>,
    ) -> Option<FunctionInstanceId> {
        // A function with no relevant template parameters has exactly one
        // instance; the site's callable type may be a coerced view of the
        // template, so the template signature is the correct request.
        let (function_type, substitutions, evidence) =
            if self.program.relevant_parameters(function).is_empty() {
                match self.program.functions.get(function) {
                    Some(template) => (
                        template.signature.clone(),
                        CallSubstitutions::default(),
                        None,
                    ),
                    None => (function_type, substitutions, evidence),
                }
            } else {
                (function_type, substitutions, evidence)
            };
        let request = InstanceResolutionRequest {
            function,
            origin: origin.clone(),
            function_type,
            substitutions,
            evidence,
            target: InstanceResolutionTarget::Root,
        };
        let resolved = match self.program.resolve_instance_request(&request) {
            Ok(resolved) => resolved,
            Err(diagnostic) => {
                self.diagnostics.push(diagnostic);
                return None;
            }
        };
        let Some(ordinal) = self.program.specializations.instance_ordinal(&resolved.key) else {
            self.report(
                origin,
                format!(
                    "initializer {} dispatch site resolves to an instance of function {} that was never interned",
                    self.initializer.index(),
                    function.0
                ),
            );
            return None;
        };
        let instance = FunctionInstanceId::from_index(ordinal.index());
        self.bindings
            .insert(site, LoweredBoundTarget::Instance(instance));
        Some(instance)
    }

    fn bind_thunk(&mut self, site: LoweredBindingSite, thunk: Option<FunctionId>, origin: &Origin) {
        let Some(function) = thunk else {
            return;
        };
        let Some(function_type) = self
            .program
            .functions
            .get(function)
            .map(|template| template.signature.clone())
        else {
            self.report(
                origin,
                format!("implicit thunk {} has no lowered template", function.0),
            );
            return;
        };
        self.bind_instance(
            site,
            origin,
            function,
            function_type,
            CallSubstitutions::default(),
            None,
        );
    }

    fn bind_call(&mut self, id: LoweredCallId, call: &LoweredCall) {
        let site = LoweredBindingSite::Call(id);
        match &call.target {
            LoweredCallableTarget::DirectFunction { function, .. } => {
                self.bind_instance(
                    site,
                    &call.origin,
                    *function,
                    call.function_type.clone(),
                    call.substitutions.clone(),
                    call.evidence.clone(),
                );
            }
            LoweredCallableTarget::TraitImplementation {
                trait_id, method, ..
            }
            | LoweredCallableTarget::StructuralTraitMethod {
                trait_id, method, ..
            } => {
                let Some(evidence) = &call.evidence else {
                    self.report(&call.origin, "trait call has no evidence recipe");
                    return;
                };
                self.bind_trait_site(
                    site,
                    &call.origin,
                    *trait_id,
                    *method,
                    evidence,
                    Some(call.function_type.clone()),
                    call.substitutions.clone(),
                );
            }
            LoweredCallableTarget::CompilerHelper { function } => {
                self.report(
                    &call.origin,
                    format!(
                        "compiler-helper target function {} has no generated artifact",
                        function.0
                    ),
                );
            }
            LoweredCallableTarget::IndirectClosure { .. }
            | LoweredCallableTarget::ExternalFunction { .. }
            | LoweredCallableTarget::Intrinsic { .. }
            | LoweredCallableTarget::Constructor { .. } => {
                self.bindings
                    .insert(site, LoweredBoundTarget::Route(call.target.category()));
            }
        }
        for (index, argument) in call.arguments.iter().enumerate() {
            if argument.thunk.is_some() {
                self.bind_thunk(
                    LoweredBindingSite::CallArgumentThunk {
                        call: id,
                        argument: index,
                    },
                    argument.thunk,
                    &call.origin,
                );
            }
        }
    }

    fn bind_callable_value(&mut self, id: LoweredCallableValueId, value: &LoweredCallableValue) {
        let site = LoweredBindingSite::CallableValue(id);
        match &value.target {
            LoweredCallableTarget::DirectFunction { function, .. } => {
                self.bind_instance(
                    site,
                    &value.origin,
                    *function,
                    value.function_type.clone(),
                    value.substitutions.clone(),
                    value.evidence.clone(),
                );
            }
            LoweredCallableTarget::TraitImplementation {
                trait_id, method, ..
            }
            | LoweredCallableTarget::StructuralTraitMethod {
                trait_id, method, ..
            } => {
                let Some(evidence) = &value.evidence else {
                    self.report(&value.origin, "trait-method value has no evidence recipe");
                    return;
                };
                self.bind_trait_site(
                    site,
                    &value.origin,
                    *trait_id,
                    *method,
                    evidence,
                    Some(value.function_type.clone()),
                    value.substitutions.clone(),
                );
            }
            LoweredCallableTarget::Constructor {
                symbol, type_id, ..
            } => {
                self.bind_constructor_adapter(
                    site,
                    *symbol,
                    *type_id,
                    value.adapter,
                    &value.function_type,
                    &value.origin,
                );
            }
            LoweredCallableTarget::CompilerHelper { function } => {
                self.report(
                    &value.origin,
                    format!(
                        "compiler-helper target function {} has no generated artifact",
                        function.0
                    ),
                );
            }
            LoweredCallableTarget::IndirectClosure { .. }
            | LoweredCallableTarget::ExternalFunction { .. }
            | LoweredCallableTarget::Intrinsic { .. } => {
                self.bindings
                    .insert(site, LoweredBoundTarget::Route(value.target.category()));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn bind_trait_site(
        &mut self,
        site: LoweredBindingSite,
        origin: &Origin,
        trait_id: TraitId,
        method: TraitMethodId,
        evidence: &TraitEvidence,
        recorded_type: Option<CheckedFunctionType>,
        substitutions: CallSubstitutions,
    ) {
        // An initializer has no enclosing instance, so the site environment is
        // built from the recipe alone.
        let site_environment = match self.program.site_environment(origin, &substitutions, None) {
            Ok(environment) => environment,
            Err(diagnostic) => {
                self.diagnostics.push(diagnostic);
                return;
            }
        };
        let resolved =
            match self
                .program
                .resolve_trait_evidence(origin, Some(evidence), &site_environment)
            {
                Ok(resolved) => resolved,
                Err(diagnostic) => {
                    self.diagnostics.push(diagnostic);
                    return;
                }
            };
        let Some(resolved) = resolved else {
            return;
        };
        match &resolved {
            TraitEvidence::ExplicitImplementation {
                function,
                arguments,
                ..
            } => {
                let function_type = match recorded_type.clone() {
                    Some(function_type) => function_type,
                    None => {
                        match instantiate_method_type(
                            self.program,
                            origin,
                            trait_id,
                            method,
                            arguments,
                        ) {
                            Ok(function_type) => function_type,
                            Err(diagnostic) => {
                                self.diagnostics.push(diagnostic);
                                return;
                            }
                        }
                    }
                };
                let evidence = if self.program.relevant_parameters(*function).is_empty() {
                    None
                } else {
                    Some(resolved.clone())
                };
                if self
                    .bind_instance(
                        site,
                        origin,
                        *function,
                        function_type,
                        substitutions,
                        evidence,
                    )
                    .is_some()
                {
                    self.evidence.insert(site, resolved);
                }
            }
            TraitEvidence::Structural {
                trait_id,
                method,
                structural,
                arguments,
            } => {
                let callable_type = match recorded_type.clone() {
                    Some(function_type) => {
                        match concretize_function_type(
                            &function_type,
                            Some(&site_environment),
                            origin,
                        ) {
                            Ok(function_type) => function_type,
                            Err(diagnostic) => {
                                self.diagnostics.push(diagnostic);
                                return;
                            }
                        }
                    }
                    None => {
                        match instantiate_method_type(
                            self.program,
                            origin,
                            *trait_id,
                            *method,
                            arguments,
                        ) {
                            Ok(function_type) => function_type,
                            Err(diagnostic) => {
                                self.diagnostics.push(diagnostic);
                                return;
                            }
                        }
                    }
                };
                match StructuralMethodKey::new(
                    *structural,
                    *trait_id,
                    *method,
                    arguments,
                    &callable_type,
                    origin,
                ) {
                    Ok(key) => {
                        let key = ArtifactRequestKey::StructuralMethod(key);
                        match self.program.specializations.artifact_ordinal(&key) {
                            Some(ordinal) => {
                                self.evidence.insert(site, resolved);
                                self.bindings
                                    .insert(site, LoweredBoundTarget::Artifact(ordinal));
                            }
                            None => self.report(
                                origin,
                                "initializer requests a structural method Stage 3.3 did not reserve",
                            ),
                        }
                    }
                    Err(diagnostic) => self.diagnostics.push(diagnostic),
                }
            }
            TraitEvidence::DeclaredBound { .. } | TraitEvidence::RejectedImplementation { .. } => {
                self.report(
                    origin,
                    "trait evidence did not resolve to a concrete selection",
                );
            }
        }
    }

    fn bind_constructor_adapter(
        &mut self,
        site: LoweredBindingSite,
        symbol: SymbolId,
        type_id: TypeId,
        adapter: super::LoweredCallableAdapter,
        callable_type: &CheckedFunctionType,
        origin: &Origin,
    ) {
        let concrete = match concretize_function_type(callable_type, None, origin) {
            Ok(concrete) => concrete,
            Err(diagnostic) => {
                self.diagnostics.push(diagnostic);
                return;
            }
        };
        match ConstructorAdapterKey::new(symbol, type_id, adapter, &concrete, origin) {
            Ok(key) => {
                let key = ArtifactRequestKey::ConstructorAdapter(key);
                match self.program.specializations.artifact_ordinal(&key) {
                    Some(ordinal) => {
                        self.bindings
                            .insert(site, LoweredBoundTarget::Artifact(ordinal));
                    }
                    None => self.report(
                        origin,
                        "initializer requests a constructor adapter Stage 3.3 did not reserve",
                    ),
                }
            }
            Err(diagnostic) => self.diagnostics.push(diagnostic),
        }
    }

    fn bind_index_site(&mut self, id: ExpressionId, index: &super::LoweredIndex) {
        let origin = self.expression_origin(id);
        let substitutions =
            trait_site_substitutions(self.program, index.trait_id, &index.arguments);
        self.bind_trait_site(
            LoweredBindingSite::Index(id),
            &origin,
            index.trait_id,
            index.dispatch.method,
            &index.evidence,
            index.method_type.clone(),
            substitutions,
        );
    }

    fn bind_formatting_sites(&mut self, id: ExpressionId, template: &LoweredStringTemplate) {
        // Literal parts go through `Formatter.write`; a template with no
        // literal part never calls it and records no instance edge.
        let write = if template
            .parts
            .iter()
            .any(|part| matches!(part, LoweredStringTemplatePart::Literal(_)))
        {
            self.program.string_formatting.write
        } else {
            None
        };
        let origin = self.expression_origin(id);
        for (function, site) in [
            (
                self.program.string_formatting.constructor,
                LoweredBindingSite::FormattingConstructor(id),
            ),
            (write, LoweredBindingSite::FormattingWrite(id)),
            (
                self.program.string_formatting.finish,
                LoweredBindingSite::FormattingFinish(id),
            ),
        ] {
            let Some(function) = function else {
                continue;
            };
            let Some(function_type) = self
                .program
                .functions
                .get(function)
                .map(|template| template.signature.clone())
            else {
                self.report(
                    &origin,
                    format!("formatter helper {} has no lowered template", function.0),
                );
                continue;
            };
            self.bind_instance(
                site,
                &origin,
                function,
                function_type,
                CallSubstitutions::default(),
                None,
            );
        }
        for (part_index, part) in template.parts.iter().enumerate() {
            let LoweredStringTemplatePart::Interpolation(interpolation) = part else {
                continue;
            };
            let substitutions = trait_site_substitutions(
                self.program,
                interpolation.trait_id,
                std::slice::from_ref(&interpolation.value_type),
            );
            self.bind_trait_site(
                LoweredBindingSite::Interpolation {
                    template: id,
                    part: part_index,
                },
                &origin,
                interpolation.trait_id,
                interpolation.method,
                &interpolation.evidence,
                None,
                substitutions,
            );
        }
    }

    fn bind_indexed_assignment_site(
        &mut self,
        id: ItemId,
        assignment: &super::LoweredAssignmentItem,
        origin: &Origin,
    ) {
        let Some(dispatch) = &assignment.mutate_index else {
            return;
        };
        let Some(evidence) = &assignment.evidence else {
            self.report(origin, "indexed assignment has no evidence recipe");
            return;
        };
        let trait_id = evidence_trait_id(evidence);
        let substitutions = trait_site_substitutions(self.program, trait_id, &dispatch.arguments);
        self.bind_trait_site(
            LoweredBindingSite::IndexedAssignment(id),
            origin,
            trait_id,
            dispatch.method,
            evidence,
            None,
            substitutions,
        );
    }

    /// Binds the thunk instances one reactive operation owns: a derived
    /// evaluator, and a reaction/`until`/`batch` callback thunk. The instance
    /// body binder binds callbacks where they are cloned; an initializer has
    /// no clone pass, so the operation site is the binding site.
    fn bind_reactive_operation(&mut self, id: LoweredReactiveOperationId) {
        let Some(operation) = self.program.reactive_operations.get(id) else {
            return;
        };
        let callback = match &operation.kind {
            LoweredReactiveOperationKind::DerivedCreate {
                evaluator,
                function_type,
                ..
            } => {
                self.bind_instance(
                    LoweredBindingSite::DerivedEvaluator(id),
                    &operation.origin,
                    *evaluator,
                    function_type.clone(),
                    CallSubstitutions::default(),
                    None,
                );
                None
            }
            LoweredReactiveOperationKind::Reaction { callback, .. } => Some(*callback),
            LoweredReactiveOperationKind::Until { predicate, .. } => Some(*predicate),
            LoweredReactiveOperationKind::Batch { callback } => Some(*callback),
            LoweredReactiveOperationKind::SignalCreate { .. }
            | LoweredReactiveOperationKind::SignalRead { .. }
            | LoweredReactiveOperationKind::SignalNotify { .. }
            | LoweredReactiveOperationKind::DerivedRead { .. }
            | LoweredReactiveOperationKind::Scope
            | LoweredReactiveOperationKind::Snapshot => None,
        };
        if let Some(callback) = callback {
            let origin = self
                .program
                .reactive_callbacks
                .get(callback)
                .map(|callback| callback.origin.clone())
                .unwrap_or_else(|| operation.origin.clone());
            let thunk = self
                .program
                .reactive_callbacks
                .get(callback)
                .and_then(|callback| callback.thunk);
            self.bind_thunk(
                LoweredBindingSite::ReactiveCallback(callback),
                thunk,
                &origin,
            );
        }
    }

    fn bind_coro(&mut self, id: LoweredCoroId) {
        let Some(coro) = self.program.coros.get(id) else {
            return;
        };
        let Some(plan) = self.program.coroutine_plans.get(coro.plan) else {
            return;
        };
        let Some(mut function_type) = self
            .program
            .functions
            .get(plan.thunk)
            .map(|function| function.signature.clone())
        else {
            self.report(&coro.origin, "coroutine body thunk has no lowered template");
            return;
        };
        function_type.effects = plan.deferred_effects.clone();
        self.bind_instance(
            LoweredBindingSite::Coro(id),
            &coro.origin,
            plan.thunk,
            function_type,
            CallSubstitutions::default(),
            None,
        );
    }

    fn bind_await_child_plan(&mut self, id: LoweredAwaitId, await_: &LoweredAwait) {
        let plan = match &await_.kind {
            super::LoweredAwaitKind::ChildCoroutine {
                plan: Some(plan), ..
            } => *plan,
            _ => return,
        };
        let owner = self
            .program
            .expressions
            .get(await_.operand)
            .and_then(|expression| match &expression.kind {
                LoweredExpressionKind::Coro(coro) => {
                    self.bindings.get(&LoweredBindingSite::Coro(*coro))
                }
                _ => None,
            })
            .and_then(LoweredBoundTarget::instance_id);
        match owner {
            Some(instance) => {
                self.bindings.insert(
                    LoweredBindingSite::AwaitChildPlan(id),
                    LoweredBoundTarget::Instance(instance),
                );
            }
            None => self.report(
                &await_.origin,
                format!(
                    "await child plan {} is not bound by a coroutine creation in this initializer",
                    plan.index()
                ),
            ),
        }
    }

    fn expression_origin(&self, id: ExpressionId) -> Origin {
        self.program
            .expressions
            .get(id)
            .map(|expression| expression.origin.clone())
            .unwrap_or_else(Origin::compiler)
    }
}

/// Reports every site whose stored entry differs from the fresh one, in site
/// order. A site present on only one side reports `None` for the other.
fn compare_tables<T: PartialEq>(
    stored: &BTreeMap<LoweredBindingSite, T>,
    fresh: &BTreeMap<LoweredBindingSite, T>,
    mut report: impl FnMut(LoweredBindingSite, Option<&T>, Option<&T>),
) {
    let sites = stored
        .keys()
        .chain(fresh.keys())
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    for site in sites {
        let (stored, fresh) = (stored.get(&site), fresh.get(&site));
        if stored != fresh {
            report(site, stored, fresh);
        }
    }
}

/// The trait a resolved evidence recipe selects. Every evidence variant names
/// its owning trait.
fn evidence_trait_id(evidence: &TraitEvidence) -> TraitId {
    match evidence {
        TraitEvidence::ExplicitImplementation { trait_id, .. }
        | TraitEvidence::Structural { trait_id, .. }
        | TraitEvidence::DeclaredBound { trait_id, .. }
        | TraitEvidence::RejectedImplementation { trait_id, .. } => *trait_id,
    }
}

impl LoweredOwnerVisitor for InitializerBinder<'_> {
    fn call_id_site(
        &mut self,
        id: LoweredCallId,
        call: &LoweredCall,
    ) -> Result<(), Vec<Diagnostic>> {
        self.bind_call(id, call);
        Ok(())
    }

    fn callable_value_site(
        &mut self,
        id: LoweredCallableValueId,
        value: &LoweredCallableValue,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        self.bind_callable_value(id, value);
        Ok(())
    }

    fn expression_site(
        &mut self,
        id: ExpressionId,
        expression: &LoweredExpression,
    ) -> Result<(), Vec<Diagnostic>> {
        match &expression.kind {
            LoweredExpressionKind::Index(index) => self.bind_index_site(id, index),
            LoweredExpressionKind::StringTemplate(template) => {
                self.bind_formatting_sites(id, template);
            }
            _ => {}
        }
        Ok(())
    }

    fn item_site(&mut self, id: ItemId, item: &LoweredItem) -> Result<(), Vec<Diagnostic>> {
        if let LoweredItemKind::Assignment(assignment) = &item.kind {
            let origin = item.origin.clone();
            self.bind_indexed_assignment_site(id, assignment, &origin);
        }
        Ok(())
    }

    fn walks_derived_binding_values(&self) -> bool {
        true
    }

    fn reactive_operation(
        &mut self,
        id: LoweredReactiveOperationId,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        self.bind_reactive_operation(id);
        Ok(())
    }

    fn await_id_site(
        &mut self,
        id: LoweredAwaitId,
        await_: &LoweredAwait,
    ) -> Result<(), Vec<Diagnostic>> {
        self.bind_await_child_plan(id, await_);
        Ok(())
    }

    fn coro_creation(
        &mut self,
        id: LoweredCoroId,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        self.bind_coro(id);
        Ok(())
    }
}

/// The validator for one initializer's binding table. It re-walks the body and
/// checks every dispatch site has the expected binding, that the table has no
/// stale keys, that closure-phase uses name the same target as their mapped
/// binding, and that every instance this initializer first requested is bound.
struct InitializerBindingValidator<'a> {
    program: &'a LoweredProgram,
    initializer: InitializerId,
    bindings: &'a BTreeMap<LoweredBindingSite, LoweredBoundTarget>,
    evidence: &'a BTreeMap<LoweredBindingSite, TraitEvidence>,
    visited: HashSet<LoweredBindingSite>,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> InitializerBindingValidator<'a> {
    fn run(mut self) -> Vec<Diagnostic> {
        let program = self.program;
        let initializer = self.initializer;
        {
            let mut visitor = InitializerBindingCheckVisitor {
                validator: &mut self,
            };
            if let Err(mut problems) =
                walk_owner(program, OwnerArenas::Initializer(initializer), &mut visitor)
            {
                self.diagnostics.append(&mut problems);
            }
        }
        // Every table key must name a live dispatch site.
        let stale = self
            .bindings
            .keys()
            .filter(|site| !self.visited.contains(site))
            .copied()
            .collect::<Vec<_>>();
        for site in stale {
            self.report(format!(
                "initializer {} binding table has a stale key {site:?}",
                initializer.index()
            ));
        }
        let stale_evidence = self
            .evidence
            .keys()
            .filter(|site| !self.bindings.contains_key(site))
            .copied()
            .collect::<Vec<_>>();
        for site in stale_evidence {
            self.report(format!(
                "initializer {} evidence table has a key {site:?} with no binding",
                initializer.index()
            ));
        }
        self.check_use_agreement();
        self.check_request_root_coverage();
        self.diagnostics
    }

    fn report(&mut self, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic::new(
            self.program.initializer_span(self.initializer),
            message.into(),
        ));
    }

    /// Checks one site's binding against the expected shape. `needs_target` is
    /// true when the site must resolve to an instance or artifact rather than
    /// keep its route.
    fn check_site(&mut self, site: LoweredBindingSite, needs_target: bool) -> bool {
        self.visited.insert(site);
        let target = self.bindings.get(&site).cloned();
        match target {
            None => {
                self.report(format!(
                    "initializer {} dispatch site {site:?} has no binding",
                    self.initializer.index()
                ));
                false
            }
            Some(LoweredBoundTarget::Route(_)) if needs_target => {
                self.report(format!(
                    "initializer {} dispatch site {site:?} is bound to a route instead of an instance or artifact",
                    self.initializer.index()
                ));
                false
            }
            Some(LoweredBoundTarget::Instance(instance)) => {
                if !self.program.instances.contains(instance) {
                    self.report(format!(
                        "initializer {} site {site:?} binds missing instance {}",
                        self.initializer.index(),
                        instance.index()
                    ));
                }
                self.evidence_or_site_kind(site);
                true
            }
            Some(LoweredBoundTarget::Artifact(ordinal)) => {
                if !self
                    .program
                    .artifacts
                    .contains(LoweredArtifactRequestId::from_index(ordinal.index()))
                {
                    self.report(format!(
                        "initializer {} site {site:?} binds missing artifact {}",
                        self.initializer.index(),
                        ordinal.index()
                    ));
                }
                true
            }
            Some(LoweredBoundTarget::Route(_)) => true,
        }
    }

    /// Trait-dependent sites carry evidence; every other site must not.
    fn evidence_or_site_kind(&mut self, site: LoweredBindingSite) {
        let trait_dependent = matches!(
            site,
            LoweredBindingSite::Index(_)
                | LoweredBindingSite::Interpolation { .. }
                | LoweredBindingSite::IndexedAssignment(_)
        ) || match site {
            LoweredBindingSite::Call(id) => self.program.calls.get(id).is_some_and(|call| {
                matches!(
                    call.target,
                    LoweredCallableTarget::TraitImplementation { .. }
                        | LoweredCallableTarget::StructuralTraitMethod { .. }
                )
            }),
            LoweredBindingSite::CallableValue(id) => {
                self.program.callable_values.get(id).is_some_and(|value| {
                    matches!(
                        value.target,
                        LoweredCallableTarget::TraitImplementation { .. }
                            | LoweredCallableTarget::StructuralTraitMethod { .. }
                    )
                })
            }
            _ => false,
        };
        if trait_dependent && !self.evidence.contains_key(&site) {
            self.report(format!(
                "initializer {} trait site {site:?} has no resolved evidence",
                self.initializer.index()
            ));
        }
        if !trait_dependent && self.evidence.contains_key(&site) {
            self.report(format!(
                "initializer {} non-trait site {site:?} carries evidence",
                self.initializer.index()
            ));
        }
    }

    /// Every closure-phase use whose site maps to a binding key must name the
    /// target the binding does.
    fn check_use_agreement(&mut self) {
        let Some(uses) = self
            .program
            .initializer_instance_uses
            .get(self.initializer.index())
        else {
            return;
        };
        for use_ in uses {
            let site = match use_.site {
                ArtifactUseSite::CoroCreation(coro) => LoweredBindingSite::Coro(coro),
                ArtifactUseSite::ReactiveCallbackEnvironment(callback) => {
                    LoweredBindingSite::ReactiveCallback(callback)
                }
                ArtifactUseSite::DerivedEvaluatorEnvironment(operation) => {
                    LoweredBindingSite::DerivedEvaluator(operation)
                }
                _ => continue,
            };
            if let Some(LoweredBoundTarget::Instance(bound)) = self.bindings.get(&site)
                && *bound != use_.instance
            {
                self.report(format!(
                    "initializer {} site {site:?} binds instance {} but its use names {}",
                    self.initializer.index(),
                    bound.index(),
                    use_.instance.index()
                ));
            }
        }
    }

    /// Every instance whose recorded request root is this initializer must be
    /// bound at one of its sites.
    fn check_request_root_coverage(&mut self) {
        let initializer = self.initializer;
        let missing = self
            .program
            .instances
            .iter()
            .filter(|(_, instance)| {
                matches!(
                    instance.request,
                    super::LoweredInstanceRequest::Initializer { initializer: owner, .. }
                        if owner == initializer
                )
            })
            .filter(|(id, _)| {
                !self
                    .bindings
                    .values()
                    .any(|target| target.instance_id() == Some(*id))
            })
            .map(|(id, instance)| {
                let template = self
                    .program
                    .functions
                    .get(instance.template)
                    .map(|function| function.name.clone())
                    .unwrap_or_else(|| format!("function {}", instance.template.0));
                (
                    id,
                    instance.origin.span.clone(),
                    template,
                    format!("{:?}", instance.request),
                )
            })
            .collect::<Vec<_>>();
        for (id, span, template, request) in missing {
            self.diagnostics.push(Diagnostic::new(
                span,
                format!(
                    "initializer {} first requested instance {} ({template}) but binds it at no site ({request})",
                    initializer.index(),
                    id.index()
                ),
            ));
        }
    }
}

struct InitializerBindingCheckVisitor<'a, 'b> {
    validator: &'b mut InitializerBindingValidator<'a>,
}

impl LoweredOwnerVisitor for InitializerBindingCheckVisitor<'_, '_> {
    fn call_id_site(
        &mut self,
        id: LoweredCallId,
        call: &LoweredCall,
    ) -> Result<(), Vec<Diagnostic>> {
        let needs_target = !matches!(
            call.target,
            LoweredCallableTarget::IndirectClosure { .. }
                | LoweredCallableTarget::ExternalFunction { .. }
                | LoweredCallableTarget::Intrinsic { .. }
                | LoweredCallableTarget::Constructor { .. }
                | LoweredCallableTarget::CompilerHelper { .. }
        );
        self.validator
            .check_site(LoweredBindingSite::Call(id), needs_target);
        for (index, argument) in call.arguments.iter().enumerate() {
            if argument.thunk.is_some() {
                self.validator.check_site(
                    LoweredBindingSite::CallArgumentThunk {
                        call: id,
                        argument: index,
                    },
                    true,
                );
            }
        }
        Ok(())
    }

    fn callable_value_site(
        &mut self,
        id: LoweredCallableValueId,
        value: &LoweredCallableValue,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        let needs_target = !matches!(
            value.target,
            LoweredCallableTarget::IndirectClosure { .. }
                | LoweredCallableTarget::ExternalFunction { .. }
                | LoweredCallableTarget::Intrinsic { .. }
                | LoweredCallableTarget::CompilerHelper { .. }
        );
        self.validator
            .check_site(LoweredBindingSite::CallableValue(id), needs_target);
        Ok(())
    }

    fn expression_site(
        &mut self,
        id: ExpressionId,
        expression: &LoweredExpression,
    ) -> Result<(), Vec<Diagnostic>> {
        match &expression.kind {
            LoweredExpressionKind::Index(_) => {
                self.validator
                    .check_site(LoweredBindingSite::Index(id), true);
            }
            LoweredExpressionKind::StringTemplate(template) => {
                self.validator
                    .check_site(LoweredBindingSite::FormattingConstructor(id), true);
                if template
                    .parts
                    .iter()
                    .any(|part| matches!(part, LoweredStringTemplatePart::Literal(_)))
                {
                    self.validator
                        .check_site(LoweredBindingSite::FormattingWrite(id), true);
                }
                self.validator
                    .check_site(LoweredBindingSite::FormattingFinish(id), true);
                for (part_index, part) in template.parts.iter().enumerate() {
                    if matches!(part, LoweredStringTemplatePart::Interpolation(_)) {
                        self.validator.check_site(
                            LoweredBindingSite::Interpolation {
                                template: id,
                                part: part_index,
                            },
                            true,
                        );
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn item_site(&mut self, id: ItemId, item: &LoweredItem) -> Result<(), Vec<Diagnostic>> {
        if let LoweredItemKind::Assignment(assignment) = &item.kind
            && assignment.mutate_index.is_some()
        {
            self.validator
                .check_site(LoweredBindingSite::IndexedAssignment(id), true);
        }
        Ok(())
    }

    fn reactive_operation(
        &mut self,
        id: LoweredReactiveOperationId,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        let Some(operation) = self.validator.program.reactive_operations.get(id) else {
            return Ok(());
        };
        let callback = match &operation.kind {
            LoweredReactiveOperationKind::DerivedCreate { .. } => {
                self.validator
                    .check_site(LoweredBindingSite::DerivedEvaluator(id), true);
                None
            }
            LoweredReactiveOperationKind::Reaction { callback, .. } => Some(*callback),
            LoweredReactiveOperationKind::Until { predicate, .. } => Some(*predicate),
            LoweredReactiveOperationKind::Batch { callback } => Some(*callback),
            _ => None,
        };
        // An explicit callable callback has no thunk and no binding site; only
        // a block callback installs a thunk.
        if let Some(callback) = callback
            && self
                .validator
                .program
                .reactive_callbacks
                .get(callback)
                .is_some_and(|callback| callback.thunk.is_some())
        {
            self.validator
                .check_site(LoweredBindingSite::ReactiveCallback(callback), true);
        }
        Ok(())
    }

    fn await_id_site(
        &mut self,
        id: LoweredAwaitId,
        await_: &LoweredAwait,
    ) -> Result<(), Vec<Diagnostic>> {
        if matches!(
            await_.kind,
            super::LoweredAwaitKind::ChildCoroutine { plan: Some(_), .. }
        ) {
            self.validator
                .check_site(LoweredBindingSite::AwaitChildPlan(id), true);
        }
        Ok(())
    }

    fn coro_creation(
        &mut self,
        id: LoweredCoroId,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        self.validator
            .check_site(LoweredBindingSite::Coro(id), true);
        Ok(())
    }

    fn walks_derived_binding_values(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use crate::{LoweredModule, Lowerer, NameResolver, ProgramLoader, TypeChecker};

    use super::*;

    fn lower(source: &str) -> LoweredModule {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("compiler crate should have a workspace parent");
        let standard_library: PathBuf = root.join("stdlib");
        let program = ProgramLoader::new()
            .with_standard_library_root(standard_library)
            .load_source(source, root)
            .expect("test source should load");
        let resolved = NameResolver::new()
            .resolve_program(program)
            .expect("test source should resolve");
        let module = TypeChecker::new()
            .check(resolved)
            .expect("test source should type check");
        Lowerer::new()
            .lower(&module)
            .unwrap_or_else(|diagnostics| panic!("source should lower: {diagnostics:?}"))
    }

    /// Initializer dispatch sites of every family the bindings cover: a
    /// derived binding reading a signal, a trait-dispatched `+`, a string
    /// template, a constructor call, a direct call, a coroutine creation, and
    /// a reaction callback.
    const INITIALIZER_SITES: &str = concat!(
        "use std.coroutine.*\n",
        "let signal flag = 0\n",
        "type Counter = ctor (value: I32)\n",
        "def add_one: I32 -> I32 = value => value + 1\n",
        "let doubled = flag + flag\n",
        "let text = \"n=${doubled}\"\n",
        "let run = add_one (doubled)\n",
        "let counter = Counter (value: doubled)\n",
        "let started = coro { 1 }\n",
        "with Reactive = reactive_scope () { reaction { () } }\n",
    );

    fn entry_initializer(program: &LoweredProgram) -> usize {
        program
            .modules
            .iter()
            .find(|(_, _, module)| module.executable_entry)
            .map(|(_, _, module)| module.initializer.index())
            .expect("the fixture has an executable entry initializer")
    }

    fn contains_message(diagnostics: &[Diagnostic], needle: &str) -> bool {
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains(needle))
    }

    #[test]
    fn initializer_sites_are_bound_and_validated() {
        let lowered = lower(INITIALIZER_SITES);
        let program = &lowered.program;
        let index = entry_initializer(program);
        let bindings = &program.initializer_bindings[index];
        assert!(
            bindings
                .values()
                .any(|target| matches!(target, LoweredBoundTarget::Instance(_))),
            "the entry initializer binds at least one instance: {bindings:?}"
        );
        let categories = bindings
            .iter()
            .filter_map(|(site, target)| match (site, target) {
                (LoweredBindingSite::Call(id), LoweredBoundTarget::Route(_)) => {
                    program.calls.get(*id).map(|call| call.target.category())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            categories.contains(&crate::LoweredCallableCategory::Constructor),
            "a constructor call keeps its route: {categories:?}"
        );
        assert!(
            bindings
                .keys()
                .any(|site| matches!(site, LoweredBindingSite::Interpolation { .. })),
            "the string template interpolation is bound"
        );
        assert!(
            bindings
                .keys()
                .any(|site| matches!(site, LoweredBindingSite::ReactiveCallback(_))),
            "the reaction callback thunk is bound"
        );
        assert!(
            bindings
                .keys()
                .any(|site| matches!(site, LoweredBindingSite::Coro(_))),
            "the coroutine creation thunk is bound"
        );
        assert!(
            program.validate_initializer_bindings().is_empty(),
            "the tables validate before corruption"
        );
    }

    /// Two sites bound to each other's (existing, initializer-requested)
    /// instances pass every structural check; only re-resolution sees it.
    #[test]
    fn swapped_bindings_are_diagnosed() {
        let mut lowered = lower(concat!(
            "def one: I32 -> I32 = value => value + 1\n",
            "def two: I32 -> I32 = value => value + 2\n",
            "let a = one (1)\n",
            "let b = two (2)\n",
        ));
        let program = &mut lowered.program;
        let index = entry_initializer(program);
        assert!(program.validate_initializer_bindings().is_empty());
        let bound = program.initializer_bindings[index]
            .iter()
            .filter_map(|(site, target)| target.instance_id().map(|instance| (*site, instance)))
            .collect::<Vec<_>>();
        let (first_site, first) = bound[0];
        let (second_site, second) = bound
            .iter()
            .copied()
            .find(|(_, instance)| *instance != first)
            .expect("the fixture binds two distinct instances");
        let table = &mut program.initializer_bindings[index];
        table.insert(first_site, LoweredBoundTarget::Instance(second));
        table.insert(second_site, LoweredBoundTarget::Instance(first));
        assert!(
            contains_message(&program.validate_initializer_bindings(), "re-resolves to"),
            "a binding to the wrong existing instance is diagnosed"
        );
    }

    #[test]
    fn a_missing_binding_is_diagnosed() {
        let mut lowered = lower(INITIALIZER_SITES);
        let program = &mut lowered.program;
        let index = entry_initializer(program);
        let site = *program.initializer_bindings[index]
            .keys()
            .find(|site| matches!(site, LoweredBindingSite::Call(_)))
            .expect("a call site");
        program.initializer_bindings[index].remove(&site);
        assert!(
            contains_message(&program.validate_initializer_bindings(), "dispatch site"),
            "a missing binding is diagnosed"
        );
    }

    #[test]
    fn a_binding_to_a_missing_instance_is_diagnosed() {
        let mut lowered = lower(INITIALIZER_SITES);
        let program = &mut lowered.program;
        let index = entry_initializer(program);
        let missing = FunctionInstanceId::from_index(program.instances.len() + 10);
        let site = *program.initializer_bindings[index]
            .iter()
            .find(|(_, target)| matches!(target, LoweredBoundTarget::Instance(_)))
            .map(|(site, _)| site)
            .expect("an instance binding");
        program.initializer_bindings[index].insert(site, LoweredBoundTarget::Instance(missing));
        assert!(
            contains_message(
                &program.validate_initializer_bindings(),
                "binds missing instance"
            ),
            "a dangling instance target is diagnosed"
        );
    }

    #[test]
    fn a_trait_site_without_evidence_is_diagnosed() {
        let mut lowered = lower(INITIALIZER_SITES);
        let program = &mut lowered.program;
        let index = entry_initializer(program);
        let site = *program.initializer_evidence[index]
            .keys()
            .next()
            .expect("a trait site with evidence");
        program.initializer_evidence[index].remove(&site);
        assert!(
            contains_message(
                &program.validate_initializer_bindings(),
                "has no resolved evidence"
            ),
            "missing evidence at a trait site is diagnosed"
        );
    }

    #[test]
    fn a_stale_binding_key_is_diagnosed() {
        let mut lowered = lower(INITIALIZER_SITES);
        let program = &mut lowered.program;
        let index = entry_initializer(program);
        let host = *program.initializer_bindings[index]
            .keys()
            .find(|site| matches!(site, LoweredBindingSite::Call(_)))
            .expect("a call site");
        let LoweredBindingSite::Call(id) = host else {
            unreachable!()
        };
        let stale = LoweredBindingSite::Call(LoweredCallId::from_index(id.index() + 10_000));
        program.initializer_bindings[index].insert(
            stale,
            LoweredBoundTarget::Route(crate::LoweredCallableCategory::IndirectClosure),
        );
        assert!(
            contains_message(&program.validate_initializer_bindings(), "stale key"),
            "a key with no live site is diagnosed"
        );
    }

    #[test]
    fn a_use_naming_another_instance_is_diagnosed() {
        let mut lowered = lower(INITIALIZER_SITES);
        let program = &mut lowered.program;
        let index = entry_initializer(program);
        let (site, bound) = program.initializer_bindings[index]
            .iter()
            .find_map(|(site, target)| match (site, target) {
                (LoweredBindingSite::Coro(_), LoweredBoundTarget::Instance(instance)) => {
                    Some((*site, *instance))
                }
                _ => None,
            })
            .expect("the fixture binds its coroutine body thunk");
        let LoweredBindingSite::Coro(coro) = site else {
            unreachable!()
        };
        let other = (0..program.instances.len())
            .map(FunctionInstanceId::from_index)
            .find(|id| *id != bound)
            .expect("another instance");
        // A closure-phase use at the bound site that names a different
        // instance is a corruption, not a Stage 4.2 failure: this table's
        // contract is that the binding and the use agree.
        program.initializer_instance_uses[index].push(crate::LoweredInstanceUse {
            site: ArtifactUseSite::CoroCreation(coro),
            instance: other,
            kind: crate::LoweredInstanceDependencyKind::CoroutineBody,
            origin: Origin::compiler(),
        });
        assert!(
            contains_message(
                &program.validate_initializer_bindings(),
                "but its use names"
            ),
            "a use/binding instance disagreement is diagnosed"
        );
    }

    #[test]
    fn an_unbound_initializer_root_request_is_diagnosed() {
        let mut lowered = lower(INITIALIZER_SITES);
        let program = &mut lowered.program;
        let index = entry_initializer(program);
        let target = program.initializer_bindings[index]
            .values()
            .find_map(LoweredBoundTarget::instance_id)
            .expect("an instance binding");
        program.initializer_bindings[index]
            .retain(|_, binding| binding.instance_id() != Some(target));
        assert!(
            contains_message(
                &program.validate_initializer_bindings(),
                "binds it at no site"
            ),
            "an unbound Initializer-rooted request is diagnosed"
        );
    }
}
