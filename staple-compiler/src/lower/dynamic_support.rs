//! Transitive support analysis for the hidden differential lowering option.
//!
//! The option lowers direct generic calls from concrete owners through
//! dynamic implementations. A dynamic implementation is only selected when
//! the callee's body, and every generic callee it would call with run-time
//! type arguments, can be emitted by the dynamic emitter. This module computes
//! that greatest fixed point over lowered templates: a function starts
//! supported and is excluded when its body uses a construct the dynamic
//! emitter does not handle, or when it depends on an excluded function.
//!
//! The same analysis produces the transitive restriction reports for
//! polymorphic values.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::lower::{
    BlockId, ExpressionId, ItemId, LoweredCallableTarget, LoweredExpressionKind, LoweredItemKind,
    LoweredPatternKind, LoweredProductStep, LoweredProgram, PatternId, PlaceId,
};
use crate::{
    CheckedFunctionType, CheckedType, FunctionId, TypeParameterId, contains_type_parameter,
};

/// Whether a function's template can be emitted as a dynamic implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DynamicSupport {
    Supported,
    Unsupported,
}

impl DynamicSupport {
    fn is_supported(self) -> bool {
        self == DynamicSupport::Supported
    }
}

/// Why one template cannot be lowered through a dynamic implementation.
/// Every reason is either in scope, meaning the dynamic emitter should
/// handle it, or allowed: a known restriction or an unsupported feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum RejectionReason {
    /// A callable value expression (`CallableValue`).
    CallableValue,
    /// Recursive (`Ref`/`Slice`/`Syntax`) construction.
    RecursiveConstruction,
    /// A deferred callable expression reached the scanner.
    Deferred,
    /// A parameter-dependent call to a non-generic function.
    ParameterDependentNonGeneric,
    /// A parameter-dependent extern call.
    ExternWithParameters,
    /// A lowered record referenced by the body is missing.
    Malformed,
    /// A template excluded only because it calls an excluded template.
    CallsExcludedTemplate,
    // Allowed reasons (unsupported features or documented restrictions).
    /// A trait obligation at run-time types that no dictionary can satisfy.
    TraitCallRuntime,
    /// An async intrinsic: completion, task, resolver.
    AsyncIntrinsic,
    /// A reactive intrinsic.
    ReactiveIntrinsic,
    /// A coroutine or `await` expression.
    CoroutineExpression,
    /// A reactive name read.
    ReactiveNameRead,
    /// A call with a reactive operation.
    ReactiveCall,
    /// A signal or derived binding.
    SignalBinding,
    /// A reactive binding record.
    ReactiveBinding,
    /// A signal assignment.
    SignalAssignment,
    /// `__buffer_clone`.
    BufferClone,
    /// A run-time `Natural` count (a restriction).
    RuntimeNaturalCount,
    /// `__slice_from_ref` (a restriction).
    SliceFromRef,
    /// A function value behind a reference, slice, buffer, or shared cell
    /// (a restriction).
    #[allow(dead_code)]
    FunctionBehindReference,
}

impl RejectionReason {
    /// Whether this reason is in scope. In-scope reasons must stay at zero;
    /// allowed reasons are unsupported features or documented restrictions.
    pub(crate) fn in_scope(self) -> bool {
        !matches!(
            self,
            RejectionReason::TraitCallRuntime
                | RejectionReason::AsyncIntrinsic
                | RejectionReason::ReactiveIntrinsic
                | RejectionReason::CoroutineExpression
                | RejectionReason::ReactiveNameRead
                | RejectionReason::ReactiveCall
                | RejectionReason::SignalBinding
                | RejectionReason::ReactiveBinding
                | RejectionReason::SignalAssignment
                | RejectionReason::BufferClone
                | RejectionReason::RuntimeNaturalCount
                | RejectionReason::SliceFromRef
                | RejectionReason::FunctionBehindReference
        )
    }

    /// The report label.
    #[allow(dead_code)]
    pub(crate) fn description(self) -> &'static str {
        match self {
            RejectionReason::CallableValue => "callable value expression",
            RejectionReason::RecursiveConstruction => "recursive construction",
            RejectionReason::Deferred => "deferred expression",
            RejectionReason::ParameterDependentNonGeneric => "parameter-dependent non-generic call",
            RejectionReason::ExternWithParameters => "parameter-dependent extern call",
            RejectionReason::Malformed => "malformed lowered record",
            RejectionReason::CallsExcludedTemplate => "calls an excluded template",
            RejectionReason::TraitCallRuntime => "trait call with run-time arguments",
            RejectionReason::AsyncIntrinsic => "async intrinsic",
            RejectionReason::ReactiveIntrinsic => "reactive intrinsic",
            RejectionReason::CoroutineExpression => "coroutine or await expression",
            RejectionReason::ReactiveNameRead => "reactive name read",
            RejectionReason::ReactiveCall => "reactive call",
            RejectionReason::SignalBinding => "signal or derived binding",
            RejectionReason::ReactiveBinding => "reactive binding",
            RejectionReason::SignalAssignment => "signal assignment",
            RejectionReason::BufferClone => "buffer clone",
            RejectionReason::RuntimeNaturalCount => "run-time Natural count",
            RejectionReason::SliceFromRef => "slice from ref",
            RejectionReason::FunctionBehindReference => "function value behind a reference",
        }
    }
}

/// Classifies an intrinsic the dynamic emitter does not support.
fn intrinsic_rejection(intrinsic: crate::IntrinsicFunction) -> RejectionReason {
    use crate::IntrinsicFunction;
    match intrinsic {
        IntrinsicFunction::BufferClone => RejectionReason::BufferClone,
        IntrinsicFunction::SliceFromRef => RejectionReason::SliceFromRef,
        IntrinsicFunction::ReactiveScope
        | IntrinsicFunction::Reaction
        | IntrinsicFunction::Batch
        | IntrinsicFunction::Snapshot
        | IntrinsicFunction::Until => RejectionReason::ReactiveIntrinsic,
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
        | IntrinsicFunction::ResolverCancel => RejectionReason::AsyncIntrinsic,
        IntrinsicFunction::NumericConvert { .. }
        | IntrinsicFunction::ParseNumber { .. }
        | IntrinsicFunction::PointerCast
        | IntrinsicFunction::PointerAddress
        | IntrinsicFunction::AddressPointer
        | IntrinsicFunction::ValidateUtf8
        | IntrinsicFunction::CStringBytes
        | IntrinsicFunction::StringHasNul
        | IntrinsicFunction::ToString { .. }
        | IntrinsicFunction::IntegerBinary { .. }
        | IntrinsicFunction::IntegerCompare { .. }
        | IntrinsicFunction::FloatBinary { .. }
        | IntrinsicFunction::FloatCompare { .. }
        | IntrinsicFunction::StringFromCString
        | IntrinsicFunction::StringToCString
        | IntrinsicFunction::StringAdd
        | IntrinsicFunction::SliceLength
        | IntrinsicFunction::SliceGetRef
        | IntrinsicFunction::BufferWithCapacity
        | IntrinsicFunction::BufferLength
        | IntrinsicFunction::BufferCapacity
        | IntrinsicFunction::BufferPush
        | IntrinsicFunction::BufferPop
        | IntrinsicFunction::BufferGet
        | IntrinsicFunction::BufferFreeze
        | IntrinsicFunction::BufferTransfer
        | IntrinsicFunction::RefReplace => {
            unreachable!("supported intrinsic passed to rejection classifier")
        }
    }
}

impl LoweredProgram {
    /// The runtime set the differential option requests for one direct call,
    /// or `None` when the call stays monomorphized. `caller_runtime` is the
    /// enclosing instance's run-time set; a call from inside a dynamic body
    /// already infers its callee's run-time parameters and is never selected
    /// here. A `Current` request must reuse the enclosing instance, so it is
    /// never rerouted.
    pub(crate) fn differential_call_runtime(
        &self,
        function: FunctionId,
        caller_runtime: &BTreeSet<TypeParameterId>,
        current: bool,
        _function_type: &CheckedFunctionType,
        substitutions: &crate::CallSubstitutions,
    ) -> Option<crate::RuntimeScope> {
        if !(self.dictionary_pass_generic_calls || self.dictionary_pass_effect_rows)
            || current
            || !caller_runtime.is_empty()
        {
            return None;
        }

        if !self.dynamic_support(function).is_supported() {
            return None;
        }
        let relevant = self.relevant_parameters(function);
        // A routed call supplies each run-time bound as a static dictionary,
        // so a bound no static dictionary serves keeps the call
        // monomorphized.
        if self.dictionary_pass_generic_calls
            && !self.static_dictionaries_available(function, &relevant, substitutions)
        {
            return None;
        }
        let scope = crate::RuntimeScope {
            types: if self.dictionary_pass_generic_calls {
                relevant.type_parameters().collect()
            } else {
                BTreeSet::new()
            },
            effects: if self.dictionary_pass_effect_rows {
                relevant.effect_parameters().collect()
            } else {
                BTreeSet::new()
            },
        };
        (!scope.is_empty()).then_some(scope)
    }

    fn static_dictionaries_available(
        &self,
        function: FunctionId,
        relevant: &crate::RelevantParameters,
        substitutions: &crate::CallSubstitutions,
    ) -> bool {
        let Some(callee) = self.functions.get(function) else {
            return true;
        };
        let runtime = relevant.type_parameters().collect::<BTreeSet<_>>();
        let substitution = substitutions
            .types
            .iter()
            .map(|entry| (entry.parameter, entry.value_type.clone()))
            .collect();
        super::trait_dictionaries::runtime_obligations(
            &self.semantic_ids,
            &callee.declared_bounds,
            &Default::default(),
            &runtime,
        )
        .into_iter()
        .map(|obligation| crate::CheckedTraitBound {
            trait_id: obligation.trait_id,
            arguments: obligation
                .arguments
                .into_iter()
                .map(|argument| crate::substitute_type(argument, &substitution))
                .collect(),
        })
        .all(|obligation| {
            !obligation.arguments.iter().any(contains_type_parameter)
                && super::trait_dictionaries::static_request(self, &obligation, &callee.origin)
                    .is_ok()
        })
    }

    /// The cached per-template support decision. Computed once after the
    /// snapshot; templates with no body are trivially supported.
    pub(crate) fn dynamic_support(&self, function: FunctionId) -> DynamicSupport {
        self.dynamic_support
            .get(&function)
            .copied()
            .unwrap_or(DynamicSupport::Supported)
    }

    /// Computes the greatest fixed point of per-template dynamic support and
    /// records every template's reasons, including the transitive one.
    pub(crate) fn compute_dynamic_support(&mut self) {
        let functions = self
            .functions
            .iter()
            .map(|(_, id, _)| id)
            .collect::<Vec<_>>();
        let mut direct = HashMap::new();
        let mut dependencies = HashMap::new();
        for function in &functions {
            let (reasons, deps) = self.scan_dynamic_support(*function);
            direct.insert(*function, reasons);
            dependencies.insert(*function, deps);
        }
        let mut support = functions
            .iter()
            .map(|function| (*function, DynamicSupport::Supported))
            .collect::<HashMap<_, _>>();
        let mut in_scope = functions
            .iter()
            .map(|function| {
                (
                    *function,
                    direct[function].iter().any(|reason| reason.in_scope()),
                )
            })
            .collect::<HashMap<_, _>>();
        loop {
            let mut changed = false;
            for function in &functions {
                if support[function].is_supported() {
                    let dependent = dependencies[function].iter().any(|dependency| {
                        !support
                            .get(dependency)
                            .copied()
                            .unwrap_or(DynamicSupport::Supported)
                            .is_supported()
                    });
                    if !direct[function].is_empty() || dependent {
                        support.insert(*function, DynamicSupport::Unsupported);
                        changed = true;
                    }
                }
                let dependency_scope = dependencies[function].iter().any(|dependency| {
                    !support
                        .get(dependency)
                        .copied()
                        .unwrap_or(DynamicSupport::Supported)
                        .is_supported()
                        && in_scope.get(dependency).copied().unwrap_or(false)
                });
                if dependency_scope && !in_scope[function] {
                    in_scope.insert(*function, true);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let mut reasons = HashMap::new();
        for function in &functions {
            let mut set = direct[function].clone();
            if set.is_empty() && !support[function].is_supported() {
                set.insert(RejectionReason::CallsExcludedTemplate);
            }
            if !set.is_empty() {
                reasons.insert(*function, set);
            }
        }
        self.dynamic_support = support;
        self.dynamic_support_reasons = reasons;
        self.dynamic_support_in_scope = in_scope;
    }

    /// Check the reachable implementation at a polymorphic construction site.
    /// Scan dependencies directly so a transitive restriction retains its
    /// specific message rather than becoming `CallsExcludedTemplate`.
    pub(crate) fn polymorphic_construction_diagnostics(
        &self,
        function: Option<FunctionId>,
        scheme: &crate::CheckedScheme,
        origin: &crate::Origin,
    ) -> Vec<staple_syntax::Diagnostic> {
        use RejectionReason::*;
        // An associated type in the scheme (`Iterator.Item Iter`) is a
        // placeholder the bound determines. Polymorphic entries do not yet
        // take it as a run-time parameter, so such a scheme is a restriction.
        let mut projection = false;
        let mut note = |ty: &CheckedType| {
            let mut found = HashSet::new();
            collect_parameter_ids(ty, &mut found);
            projection |= found
                .iter()
                .any(|id| id.0 >= crate::typecheck::PROJECTION_PARAMETER_BASE);
        };
        note(&scheme.body);
        for bound in &scheme.trait_bounds {
            for argument in &bound.arguments {
                note(argument);
            }
        }
        if projection {
            return vec![staple_syntax::Diagnostic::new(
                origin.span.clone(),
                "polymorphic values whose type names an associated type (such as `Iterator.Item Iter`) are not supported yet",
            )];
        }
        let mut reasons = BTreeSet::new();
        // Bounds become hidden dictionaries; only a `Natural` bound,
        // which constrains compile-time counts, keeps its restriction.
        if scheme
            .trait_bounds
            .iter()
            .any(|bound| Some(bound.trait_id) == self.semantic_ids.natural_trait)
        {
            reasons.insert(RuntimeNaturalCount);
        }
        let mut pending = function.into_iter().collect::<Vec<_>>();
        let mut visited = HashSet::new();
        while let Some(function) = pending.pop() {
            if !visited.insert(function) {
                continue;
            }
            let (direct, dependencies) = self.scan_dynamic_support(function);
            reasons.extend(direct);
            pending.extend(dependencies);
        }
        let messages = reasons.into_iter().map(|reason| match reason {
            TraitCallRuntime => "a trait obligation of this polymorphic value's implementation has no dictionary at run time (it needs a bound in scope, or exactly one matching implementation)".to_owned(),
            CoroutineExpression => "functions with coroutine bodies cannot be used as polymorphic values yet".to_owned(),
            AsyncIntrinsic => "functions with async bodies cannot be used as polymorphic values yet".to_owned(),
            ReactiveIntrinsic | ReactiveNameRead | ReactiveCall | SignalBinding | ReactiveBinding | SignalAssignment => "functions with reactive bodies cannot be used as polymorphic values yet".to_owned(),
            BufferClone => "`__buffer_clone` cannot be used in a polymorphic value's implementation yet".to_owned(),
            RuntimeNaturalCount | SliceFromRef => "array lengths cannot depend on a polymorphic value's type parameters yet".to_owned(),
            FunctionBehindReference => "function values inside references cannot be passed to a polymorphic value's implementation".to_owned(),
            reason => format!("internal invariant violated: polymorphic implementation rejected for {}", reason.description()),
        }).collect::<BTreeSet<_>>();
        messages
            .into_iter()
            .map(|message| staple_syntax::Diagnostic::new(origin.span.clone(), message))
            .collect()
    }

    /// One template's direct reasons and the generic callees its dynamic
    /// implementation would call with run-time arguments.
    fn scan_dynamic_support(
        &self,
        function: FunctionId,
    ) -> (BTreeSet<RejectionReason>, HashSet<FunctionId>) {
        let mut walker = SupportWalker {
            program: self,
            declared: self
                .functions
                .get(function)
                .map(|template| template.declared_bounds.clone())
                .unwrap_or_default(),
            indexed_assignment: false,
            reasons: BTreeSet::new(),
            dependencies: HashSet::new(),
            visited_blocks: HashSet::new(),
            visited_items: HashSet::new(),
            visited_expressions: HashSet::new(),
            visited_patterns: HashSet::new(),
            visited_places: HashSet::new(),
            visited_calls: HashSet::new(),
        };
        let Some(template) = self.functions.get(function) else {
            walker.reject(RejectionReason::Malformed);
            return (walker.reasons, walker.dependencies);
        };
        // Captures are supported by value, by borrow, or through a shared cell,
        // for concrete and compound run-time types and function values.
        // Initialization-state cells are carried as shared cells.
        // A template's own effect variables are substituted by its instance
        // key, or arrive as a run-time bundle, so they are not restrictions.
        // A resource effect row is an ABI fact, not a restriction. The
        // dynamic ABI passes each resource provider as a pointer.

        if let Some(reason) = signature_function_restriction(&template.signature) {
            walker.reject(reason);
        }
        // A bound becomes a hidden dictionary. What a body does
        // with it is checked at each trait-dependent site.
        if let Some(body) = template.body {
            walker.block(body);
        }
        (walker.reasons, walker.dependencies)
    }
}

/// The scalar intrinsics the dynamic emitter handles without the GC or
/// run-time descriptors.
fn dynamic_scalar_intrinsic(intrinsic: crate::IntrinsicFunction) -> bool {
    use crate::IntrinsicFunction;
    matches!(
        intrinsic,
        IntrinsicFunction::ParseNumber { .. }
            | IntrinsicFunction::ValidateUtf8
            | IntrinsicFunction::StringHasNul
            | IntrinsicFunction::CStringBytes
            | IntrinsicFunction::ToString { .. }
            | IntrinsicFunction::StringFromCString
            | IntrinsicFunction::StringToCString
            | IntrinsicFunction::StringAdd
            | IntrinsicFunction::NumericConvert { .. }
            | IntrinsicFunction::PointerCast
            | IntrinsicFunction::PointerAddress
            | IntrinsicFunction::AddressPointer
            | IntrinsicFunction::IntegerBinary { .. }
            | IntrinsicFunction::IntegerCompare { .. }
            | IntrinsicFunction::FloatBinary { .. }
            | IntrinsicFunction::FloatCompare { .. }
            | IntrinsicFunction::SliceLength
            | IntrinsicFunction::SliceGetRef
            | IntrinsicFunction::RefReplace
            | IntrinsicFunction::BufferLength
            | IntrinsicFunction::BufferCapacity
            | IntrinsicFunction::BufferWithCapacity
            | IntrinsicFunction::BufferPush
            | IntrinsicFunction::BufferPop
            | IntrinsicFunction::BufferGet
            | IntrinsicFunction::BufferFreeze
            | IntrinsicFunction::BufferTransfer
    )
}

/// Whether a type mentions a function type that still contains a run-time
/// parameter.
fn contains_polymorphic_function(ty: &CheckedType) -> bool {
    match ty {
        CheckedType::Function(_) => contains_type_parameter(ty),
        CheckedType::CPointer { pointee } => contains_polymorphic_function(pointee),
        CheckedType::Ref(value) | CheckedType::Slice(value) | CheckedType::Buffer(value) => {
            contains_polymorphic_function(value)
        }
        CheckedType::Array { element, count } => {
            contains_polymorphic_function(element) || contains_polymorphic_function(count)
        }
        CheckedType::Opaque { arguments, .. } | CheckedType::TypeConstructor { arguments, .. } => {
            arguments.iter().any(contains_polymorphic_function)
        }
        CheckedType::Product(product) => product
            .elements
            .iter()
            .any(|field| contains_polymorphic_function(&field.value_type)),
        CheckedType::Sum(sum) => sum.alternatives.iter().any(contains_polymorphic_function),
        CheckedType::Wrapper {
            arguments,
            representation,
            ..
        } => {
            arguments.iter().any(contains_polymorphic_function)
                || contains_polymorphic_function(representation)
        }
        _ => false,
    }
}

/// Whether a type contains a function value behind a reference, slice,
/// buffer, or shared cell. Converting such a value needs copying shared
/// storage, so it is reported as an allowed restriction instead.
fn contains_function_behind_reference(ty: &CheckedType) -> bool {
    match ty {
        CheckedType::Ref(value) | CheckedType::Slice(value) | CheckedType::Buffer(value) => {
            contains_polymorphic_function(value) || contains_function_behind_reference(value)
        }
        CheckedType::Product(product) => product
            .elements
            .iter()
            .any(|field| contains_function_behind_reference(&field.value_type)),
        CheckedType::Sum(sum) => sum
            .alternatives
            .iter()
            .any(contains_function_behind_reference),
        CheckedType::Wrapper {
            arguments,
            representation,
            ..
        } => {
            arguments.iter().any(contains_function_behind_reference)
                || contains_function_behind_reference(representation)
        }
        CheckedType::Array { element, .. } => contains_function_behind_reference(element),
        CheckedType::Opaque { arguments, .. } | CheckedType::TypeConstructor { arguments, .. } => {
            arguments.iter().any(contains_function_behind_reference)
        }
        _ => false,
    }
}

/// Shared storage cannot be rewritten while adapting an owned function value.
fn signature_function_restriction(function_type: &CheckedFunctionType) -> Option<RejectionReason> {
    super::flattened_parameter_types(&function_type.parameter)
        .iter()
        .chain(std::iter::once(function_type.result.as_ref()))
        .any(contains_function_behind_reference)
        .then_some(RejectionReason::FunctionBehindReference)
}

struct SupportWalker<'program> {
    program: &'program LoweredProgram,
    /// The scanned template's declared bounds: the dictionaries its dynamic
    /// instance receives.
    declared: Vec<crate::CheckedTraitBound>,
    /// The `MutateIndex` assignment being walked has a dictionary, so its
    /// indexed target place needs no further justification.
    indexed_assignment: bool,
    reasons: BTreeSet<RejectionReason>,
    dependencies: HashSet<FunctionId>,
    visited_blocks: HashSet<BlockId>,
    visited_items: HashSet<ItemId>,
    visited_expressions: HashSet<ExpressionId>,
    visited_patterns: HashSet<PatternId>,
    visited_places: HashSet<PlaceId>,
    visited_calls: HashSet<crate::lower::LoweredCallId>,
}

impl SupportWalker<'_> {
    fn reject(&mut self, reason: RejectionReason) {
        self.reasons.insert(reason);
    }

    /// Whether a dictionary for an obligation is obtainable here: from
    /// this template's own bounds, as a static dictionary when concrete, or
    /// built from the one implementation it selects. Built members become
    /// dependencies, since each must itself run dynamically.
    fn obligation_supplied(
        &mut self,
        obligation: &crate::CheckedTraitBound,
        origin: &crate::Origin,
    ) -> bool {
        let selection = match crate::lower::trait_dictionaries::select_dictionary(
            self.program,
            &self.declared,
            obligation,
            origin,
        ) {
            Ok(selection) => selection,
            Err(_) => return false,
        };
        let mut supplied = true;
        let mut members = Vec::new();
        crate::lower::trait_dictionaries::for_each_member(&selection, &mut |member| {
            members.push(member.function)
        });
        for function in members {
            // A member with no type parameters runs concretely and would need
            // a reabstraction slot, which built dictionaries do not have yet.
            if self
                .program
                .relevant_parameters(function)
                .type_parameters()
                .next()
                .is_none()
            {
                supplied = false;
            }
            self.dependencies.insert(function);
        }
        supplied
    }

    /// Whether a trait-member call at run-time types can read a dictionary.
    fn trait_call_has_dictionary(
        &mut self,
        evidence: Option<&crate::TraitEvidence>,
        origin: &crate::Origin,
    ) -> bool {
        let Some(evidence) = evidence else {
            return false;
        };
        let (trait_id, arguments, _) = crate::lower::trait_dictionaries::evidence_obligation(evidence);
        let obligation = crate::CheckedTraitBound {
            trait_id,
            arguments: arguments.to_vec(),
        };
        self.obligation_supplied(&obligation, origin)
    }

    /// A dynamic callee's dictionaries must be suppliable here: from this
    /// template's own, built from implementations, or as static ones when the
    /// obligation is concrete. `scheme_bounds` are the bounds of a
    /// polymorphic value being built from the callee: its callers supply
    /// them, so they are not obligations here.
    fn callee_bounds_supplied(
        &mut self,
        function: FunctionId,
        substitutions: &crate::CallSubstitutions,
        scheme_bounds: &[crate::CheckedTraitBound],
    ) {
        let Some(callee) = self.program.functions.get(function) else {
            return;
        };
        let origin = callee.origin.clone();
        let substitution = substitutions
            .types
            .iter()
            .map(|entry| (entry.parameter, entry.value_type.clone()))
            .collect::<std::collections::HashMap<_, _>>();
        let runtime = self
            .program
            .relevant_parameters(function)
            .type_parameters()
            .collect::<BTreeSet<_>>();
        let declared = callee
            .declared_bounds
            .iter()
            .filter(|bound| {
                !scheme_bounds
                    .iter()
                    .any(|own| crate::lower::trait_dictionaries::bounds_equivalent(own, bound))
            })
            .cloned()
            .collect::<Vec<_>>();
        let obligations = crate::lower::trait_dictionaries::runtime_obligations(
            &self.program.semantic_ids,
            &declared,
            &Default::default(),
            &runtime,
        );
        for bound in obligations {
            let obligation = crate::CheckedTraitBound {
                trait_id: bound.trait_id,
                arguments: bound
                    .arguments
                    .into_iter()
                    .map(|argument| crate::substitute_type(argument, &substitution))
                    .collect(),
            };
            if !self.obligation_supplied(&obligation, &origin) {
                self.reject(RejectionReason::TraitCallRuntime);
            }
        }
    }

    fn block(&mut self, id: BlockId) {
        if !self.visited_blocks.insert(id) {
            return;
        }
        let Some(block) = self.program.blocks.get(id) else {
            self.reject(RejectionReason::Malformed);
            return;
        };
        let items = block.items.clone();
        let result = block.result;
        for item in items {
            self.item(item);
        }
        if let Some(result) = result {
            self.expression(result);
        }
    }

    fn item(&mut self, id: ItemId) {
        if !self.visited_items.insert(id) {
            return;
        }
        let Some(item) = self.program.items.get(id) else {
            self.reject(RejectionReason::Malformed);
            return;
        };
        match &item.kind {
            LoweredItemKind::Binding(binding) => {
                if binding.signal || binding.derived {
                    self.reject(RejectionReason::SignalBinding);
                }
                if binding.generic || binding.compile_time_only {
                    return;
                }
                if binding.reactive.is_some() {
                    self.reject(RejectionReason::ReactiveBinding);
                }
                if let Some(value) = binding.value {
                    self.expression(value);
                }
            }
            LoweredItemKind::PatternBinding(binding) => {
                self.expression(binding.value);
            }
            LoweredItemKind::Assignment(assignment) => {
                let mut supplied = false;
                if let Some(dispatch) = assignment.mutate_index.as_ref()
                    && dispatch.arguments.iter().any(contains_type_parameter)
                {
                    let origin = item.origin.clone();
                    supplied = self
                        .trait_call_has_dictionary(assignment.evidence.as_ref(), &origin);
                    if !supplied {
                        self.reject(RejectionReason::TraitCallRuntime);
                    }
                }
                if assignment.signal_notify.is_some() {
                    self.reject(RejectionReason::SignalAssignment);
                }
                self.indexed_assignment = supplied;
                self.place(assignment.target);
                self.indexed_assignment = false;
                self.expression(assignment.value);
            }
            LoweredItemKind::Return(item) => self.expression(item.value),
            LoweredItemKind::Break(item) => {
                if let Some(value) = item.value {
                    self.expression(value);
                }
            }
            LoweredItemKind::Continue(_) => {}
            LoweredItemKind::Expression(item) => self.expression(item.expression),
        }
    }

    fn expression(&mut self, id: ExpressionId) {
        if !self.visited_expressions.insert(id) {
            return;
        }
        let Some(expression) = self.program.expressions.get(id) else {
            self.reject(RejectionReason::Malformed);
            return;
        };
        if contains_function_behind_reference(&expression.value_type) {
            self.reject(RejectionReason::FunctionBehindReference);
        }
        if let Some(coercion) = &expression.coercion {
            for ty in [&coercion.source, &coercion.target] {
                if contains_function_behind_reference(ty) {
                    self.reject(RejectionReason::FunctionBehindReference);
                }
            }
        }
        match &expression.kind {
            LoweredExpressionKind::String(_) | LoweredExpressionKind::CString(_) => {}
            LoweredExpressionKind::Deferred(_) => self.reject(RejectionReason::Deferred),
            LoweredExpressionKind::StringTemplate(template) => {
                // Concrete interpolations use the concrete formatter ABI. A
                // run-time-typed interpolation needs a `Display`/`Debug`
                // dictionary, and is rejected only when none is available.
                let origin = expression.origin.clone();
                for part in &template.parts {
                    if let crate::LoweredStringTemplatePart::Interpolation(interpolation) = part {
                        if crate::contains_type_parameter(&interpolation.value_type)
                            && !self.trait_call_has_dictionary(Some(&interpolation.evidence), &origin)
                        {
                            self.reject(RejectionReason::TraitCallRuntime);
                        }
                        self.expression(interpolation.expression);
                    }
                }
            }
            LoweredExpressionKind::Index(index) => {
                if index.arguments.iter().any(contains_type_parameter)
                    && !self.trait_call_has_dictionary(Some(&index.evidence), &expression.origin)
                {
                    self.reject(RejectionReason::TraitCallRuntime);
                }
                self.expression(index.base);
                self.expression(index.index);
            }
            LoweredExpressionKind::CallableValue(value) => {
                // A closure is built either with the concrete ABI or as a
                // dynamic-shape closure pointing at a descriptor-bound entry
                // for a generic `def` or trait implementation, captures
                // included.
                let Some(callable) = self.program.callable_values.get(*value) else {
                    self.reject(RejectionReason::Malformed);
                    return;
                };
                let captures = callable
                    .closure
                    .as_ref()
                    .map_or(&[][..], |closure| closure.captures.as_slice());
                // A generic binding's cell holds only its initialization state,
                // never a function value, so a closure capturing it (a local
                // generic `def` referring to itself) converts nothing.
                let shared_function = |capture: &crate::LoweredClosureCapture| {
                    capture.access == crate::LoweredCaptureAccess::SharedCell
                        && contains_polymorphic_function(&capture.value_type)
                        && !self
                            .program
                            .symbols
                            .get(capture.capture.symbol)
                            .is_some_and(|symbol| symbol.initialization_state_only)
                };
                if captures.iter().any(shared_function) {
                    // Shared storage cannot be copied into an adapted closure's
                    // ABI without breaking aliasing.
                    self.reject(RejectionReason::FunctionBehindReference);
                }
                let dynamic_shape =
                    contains_type_parameter(&CheckedType::Function(callable.function_type.clone()));
                let dynamic_target = matches!(
                    callable.target,
                    LoweredCallableTarget::DirectFunction { .. }
                        | LoweredCallableTarget::TraitImplementation { .. }
                        | LoweredCallableTarget::Constructor {
                            recursive: None | Some(crate::RecursiveConstruction::ManagedReference),
                            ..
                        }
                );
                // A callable value whose target template cannot be lowered
                // dynamically is a transitive dependency.
                if let LoweredCallableTarget::DirectFunction { function, .. }
                | LoweredCallableTarget::TraitImplementation {
                    function: Some(function),
                    ..
                } = &callable.target
                    && self
                        .program
                        .relevant_parameters(*function)
                        .type_parameters()
                        .next()
                        .is_some()
                {
                    self.dependencies.insert(*function);
                    self.callee_bounds_supplied(
                        *function,
                        &callable.substitutions,
                        callable
                            .scheme
                            .as_ref()
                            .map_or(&[][..], |scheme| scheme.trait_bounds.as_slice()),
                    );
                }
                if dynamic_shape && !dynamic_target {
                    self.reject(RejectionReason::CallableValue);
                }
                // A trait member used as a value at a run-time type is a
                // closure over a dictionary slot.
                if matches!(
                    callable.target,
                    LoweredCallableTarget::TraitImplementation { .. }
                        | LoweredCallableTarget::StructuralTraitMethod { .. }
                ) && callable.scheme.is_none()
                    && (dynamic_shape
                        || callable.evidence.as_ref().is_some_and(|evidence| {
                            crate::lower::trait_dictionaries::evidence_obligation(evidence)
                                .1
                                .iter()
                                .any(contains_type_parameter)
                        }))
                    && !self.trait_call_has_dictionary(callable.evidence.as_ref(), &callable.origin)
                {
                    self.reject(RejectionReason::TraitCallRuntime);
                }
            }
            LoweredExpressionKind::Resource(_) => {}
            LoweredExpressionKind::With(with_id) => {
                if let Some(with) = self.program.withs.get(*with_id) {
                    if with.scope_exit != crate::lower::LoweredScopeExit::Ordinary {
                        self.reject(RejectionReason::ReactiveCall);
                    }
                    self.expression(with.value);
                    self.block(with.body);
                }
            }
            LoweredExpressionKind::Coro(_) | LoweredExpressionKind::Await(_) => {
                self.reject(RejectionReason::CoroutineExpression)
            }
            LoweredExpressionKind::Block(block) => self.block(*block),
            LoweredExpressionKind::Name(name) => {
                if name.reactive.is_some() {
                    self.reject(RejectionReason::ReactiveNameRead);
                }
            }
            LoweredExpressionKind::Integer(_) | LoweredExpressionKind::Float(_) => {}
            LoweredExpressionKind::Access(access) => {
                self.expression(access.base);
            }
            LoweredExpressionKind::Product(product) => {
                for step in &product.steps {
                    match step {
                        LoweredProductStep::Positional { expression, .. }
                        | LoweredProductStep::Designated { expression, .. }
                        | LoweredProductStep::Default { expression, .. } => {
                            self.expression(*expression);
                        }
                        LoweredProductStep::PositionalSpread { expression, .. }
                        | LoweredProductStep::NamedSpread { expression, .. } => {
                            self.expression(*expression)
                        }
                    }
                }
                for field in &product.fields {
                    self.expression(*field);
                }
            }
            LoweredExpressionKind::RepeatedProduct(product) => {
                if matches!(product.count, crate::lower::LoweredRepeatCount::Symbolic(_)) {
                    self.reject(RejectionReason::RuntimeNaturalCount);
                }
                self.expression(product.expression);
            }
            LoweredExpressionKind::TypeAscription(ascription) => self.expression(ascription.value),
            LoweredExpressionKind::Logical(logical) => {
                self.expression(logical.left);
                self.expression(logical.right);
            }
            LoweredExpressionKind::Loop(loop_) => self.block(loop_.body),
            LoweredExpressionKind::Match(match_) => {
                self.expression(match_.subject);
                for arm in &match_.arms {
                    self.pattern(arm.pattern);
                    self.expression(arm.body);
                }
            }
            LoweredExpressionKind::Call(call) => self.call(*call),
        }
    }

    fn pattern(&mut self, id: PatternId) {
        if !self.visited_patterns.insert(id) {
            return;
        }
        let Some(pattern) = self.program.patterns.get(id) else {
            self.reject(RejectionReason::Malformed);
            return;
        };
        match &pattern.kind {
            LoweredPatternKind::Literal { .. } => {}
            LoweredPatternKind::Wildcard | LoweredPatternKind::Binding { .. } => {}
            LoweredPatternKind::Product { elements, .. } => {
                for element in elements {
                    self.pattern(*element);
                }
            }
            LoweredPatternKind::Nominal { argument, .. } => {
                self.pattern(*argument);
            }
            LoweredPatternKind::At {
                binding,
                pattern: nested,
            } => {
                self.pattern(*binding);
                self.pattern(*nested);
            }
        }
    }

    fn place(&mut self, id: PlaceId) {
        if !self.visited_places.insert(id) {
            return;
        }
        let Some(place) = self.program.places.get(id) else {
            self.reject(RejectionReason::Malformed);
            return;
        };
        match &place.kind {
            crate::lower::LoweredPlaceKind::Symbol { .. }
            | crate::lower::LoweredPlaceKind::CapturedCell { .. } => {}
            crate::lower::LoweredPlaceKind::Temporary { expression } => {
                self.expression(*expression)
            }
            crate::lower::LoweredPlaceKind::Resource { .. } => {}
            crate::lower::LoweredPlaceKind::Dereference { reference, .. } => {
                self.expression(*reference)
            }
            crate::lower::LoweredPlaceKind::ProductElement { base, .. } => self.place(*base),
            crate::lower::LoweredPlaceKind::Representation { base } => self.place(*base),
            crate::lower::LoweredPlaceKind::Indexed { base, index } => {
                let runtime = self
                    .program
                    .places
                    .get(*base)
                    .is_some_and(|place| contains_type_parameter(&place.value_type))
                    || self
                        .program
                        .expressions
                        .get(*index)
                        .is_some_and(|expression| contains_type_parameter(&expression.value_type));
                if runtime && !self.indexed_assignment {
                    self.reject(RejectionReason::TraitCallRuntime);
                }
                self.place(*base);
                self.expression(*index);
            }
        }
    }

    fn call(&mut self, id: crate::lower::LoweredCallId) {
        if !self.visited_calls.insert(id) {
            return;
        }
        let Some(call) = self.program.calls.get(id) else {
            self.reject(RejectionReason::Malformed);
            return;
        };
        if call.reactive.is_some() {
            self.reject(RejectionReason::ReactiveCall);
        }
        if let Some(reason) = signature_function_restriction(&call.function_type) {
            self.reject(reason);
        }
        for entry in &call.substitutions.types {
            if contains_function_behind_reference(&entry.value_type) {
                self.reject(RejectionReason::FunctionBehindReference);
            }
        }
        for step in &call.steps {
            match step {
                crate::lower::LoweredCallStep::Callee { expression } => {
                    self.expression(*expression)
                }
                crate::lower::LoweredCallStep::Argument { argument } => {
                    self.argument(call, *argument)
                }
                crate::lower::LoweredCallStep::ProductElement { expression, .. }
                | crate::lower::LoweredCallStep::ProductSpread { expression, .. }
                | crate::lower::LoweredCallStep::NamedProductSpread { expression, .. }
                | crate::lower::LoweredCallStep::Default { expression, .. } => {
                    self.expression(*expression)
                }
                crate::lower::LoweredCallStep::Resource { .. } => {}
                crate::lower::LoweredCallStep::Invoke => {}
            }
        }
        for index in 0..call.arguments.len() {
            self.argument(call, index);
        }
        let mentions_parameters =
            contains_type_parameter(&CheckedType::Function(call.function_type.clone()))
                || call
                    .substitutions
                    .types
                    .iter()
                    .any(|entry| contains_type_parameter(&entry.value_type));
        match &call.target {
            LoweredCallableTarget::DirectFunction { function, .. } => {
                if mentions_parameters {
                    let relevant = self.program.relevant_parameters(*function);
                    let generic = relevant.type_parameters().next().is_some()
                        || relevant.effect_parameters().next().is_some();
                    if generic {
                        self.dependencies.insert(*function);
                        self.callee_bounds_supplied(*function, &call.substitutions, &[]);
                    } else if contains_type_parameter(&CheckedType::Function(
                        call.function_type.clone(),
                    )) {
                        self.reject(RejectionReason::ParameterDependentNonGeneric);
                    }
                    // Substitutions for unused parameters do not change the
                    // callee ABI or body; a fully concrete target stays native.
                }
            }
            LoweredCallableTarget::Constructor { recursive, .. } => {
                if recursive.is_some()
                    && !matches!(
                        recursive,
                        Some(crate::RecursiveConstruction::ManagedReference)
                    )
                {
                    self.reject(RejectionReason::RecursiveConstruction);
                }
            }
            // A concrete trait call dispatches to a monomorphized implementation
            // and needs no dictionary; a parameter-dependent one needs one.
            LoweredCallableTarget::TraitImplementation { .. }
            | LoweredCallableTarget::StructuralTraitMethod { .. } => {
                if mentions_parameters
                    && !self.trait_call_has_dictionary(call.evidence.as_ref(), &call.origin)
                {
                    self.reject(RejectionReason::TraitCallRuntime);
                }
            }
            // A concrete function type uses the concrete ABI, and a
            // dynamic-shape closure uses the dynamic shape without descriptor
            // parameters. Both are supported.
            LoweredCallableTarget::IndirectClosure { .. } => {}
            LoweredCallableTarget::ExternalFunction { .. } => {
                if mentions_parameters {
                    self.reject(RejectionReason::ExternWithParameters);
                }
            }
            LoweredCallableTarget::Intrinsic { intrinsic, .. } => {
                if *intrinsic == crate::IntrinsicFunction::BufferClone
                    && let CheckedType::Buffer(element) = &call.result_type
                    && contains_type_parameter(element)
                {
                    // Elements clone through the element type's dictionary.
                    let supplied = self.program.semantic_ids.clone_trait.is_some_and(|trait_id| {
                        self.obligation_supplied(
                            &crate::CheckedTraitBound {
                                trait_id,
                                arguments: vec![element.as_ref().clone()],
                            },
                            &call.origin,
                        )
                    });
                    if !supplied {
                        self.reject(RejectionReason::BufferClone);
                    }
                } else if !dynamic_scalar_intrinsic(*intrinsic) {
                    self.reject(intrinsic_rejection(*intrinsic));
                }
            }
        }
    }

    fn argument(&mut self, call: &crate::lower::LoweredCall, index: usize) {
        let Some(argument) = call.arguments.get(index) else {
            return;
        };
        if let Some(thunk) = argument.thunk {
            self.dependencies.insert(thunk);
        }
        if let Some(expression) = argument.expression {
            self.expression(expression);
        }
        if let Some(place) = argument.place {
            self.place(place);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::codegen::corpus::{CorpusSource, codegen_corpus};
    use crate::{Lowerer, NameResolver, ProgramLoader, TypeChecker};

    /// The in-scope rejection count must never rise above this baseline.
    const IN_SCOPE_BASELINE: usize = 0;

    /// Every corpus program with the directory its `use` paths resolve
    /// against.
    fn corpus_sources() -> Vec<(String, PathBuf)> {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        codegen_corpus()
            .iter()
            .map(|program| match program.source {
                CorpusSource::Inline(source) => (source.to_owned(), workspace.to_path_buf()),
                CorpusSource::File(path) => {
                    let full = workspace.join(path);
                    let source = std::fs::read_to_string(&full)
                        .unwrap_or_else(|error| panic!("`{path}` should read: {error}"));
                    (source, full.parent().unwrap().to_path_buf())
                }
            })
            .collect()
    }

    #[test]
    fn ordinary_thunks_are_selected_inside_dynamic_bodies() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let program = codegen_corpus()
            .iter()
            .find(|program| program.name == "dynamic_implicit_thunks")
            .unwrap();
        let CorpusSource::Inline(source) = program.source else {
            panic!("thunk regression is inline");
        };
        let loaded = ProgramLoader::new()
            .with_standard_library_root(workspace.join("stdlib"))
            .load_source(source, workspace)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(loaded).unwrap();
        let checked = TypeChecker::new().check(resolved).unwrap();
        let lowered = Lowerer::new()
            .with_dictionary_pass_generic_calls(true)
            .lower(&checked)
            .unwrap();
        for name in ["captured", "concrete", "indirect"] {
            let function = lowered
                .program
                .functions
                .iter()
                .find_map(|(_, id, function)| (function.name == name).then_some(id))
                .unwrap();
            assert_eq!(
                lowered.program.dynamic_support(function),
                DynamicSupport::Supported,
                "{name}"
            );
            assert!(
                lowered.program.instances.iter().any(|(_, instance)| {
                    instance.template == function
                        && lowered
                            .program
                            .specializations
                            .instance(instance.ordinal)
                            .is_some_and(|key| !key.runtime().is_empty())
                }),
                "{name} must run dynamically rather than fall back"
            );
        }
    }

    #[test]
    fn concrete_dispatch_is_selected_inside_dynamic_bodies() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let program = codegen_corpus()
            .iter()
            .find(|program| program.name == "dynamic_concrete_dispatch")
            .unwrap();
        let CorpusSource::Inline(source) = program.source else {
            panic!("dispatch regression is inline");
        };
        let loaded = ProgramLoader::new()
            .with_standard_library_root(workspace.join("stdlib"))
            .load_source(source, workspace)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(loaded).unwrap();
        let checked = TypeChecker::new().check(resolved).unwrap();
        let lowered = Lowerer::new()
            .with_dictionary_pass_generic_calls(true)
            .lower(&checked)
            .unwrap();
        for name in ["indexed", "sliced", "structural"] {
            let function = lowered
                .program
                .functions
                .iter()
                .find_map(|(_, id, function)| (function.name == name).then_some(id))
                .unwrap();
            assert_eq!(
                lowered.program.dynamic_support(function),
                DynamicSupport::Supported,
                "{name}"
            );
            assert!(
                lowered.program.instances.iter().any(|(_, instance)| {
                    instance.template == function
                        && lowered
                            .program
                            .specializations
                            .instance(instance.ordinal)
                            .is_some_and(|key| !key.runtime().is_empty())
                }),
                "{name} must run dynamically rather than fall back"
            );
        }
    }

    #[test]
    fn constructor_values_are_selected_inside_dynamic_bodies() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let program = codegen_corpus()
            .iter()
            .find(|program| program.name == "dynamic_constructor_values")
            .unwrap();
        let CorpusSource::Inline(source) = program.source else {
            panic!("constructor regression is inline");
        };
        let loaded = ProgramLoader::new()
            .with_standard_library_root(workspace.join("stdlib"))
            .load_source(source, workspace)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(loaded).unwrap();
        let checked = TypeChecker::new().check(resolved).unwrap();
        let lowered = Lowerer::new()
            .with_dictionary_pass_generic_calls(true)
            .lower(&checked)
            .unwrap();
        for name in ["wrapped", "maker", "pair_maker"] {
            let function = lowered
                .program
                .functions
                .iter()
                .find_map(|(_, id, function)| (function.name == name).then_some(id))
                .unwrap();
            assert_eq!(
                lowered.program.dynamic_support(function),
                DynamicSupport::Supported,
                "{name}"
            );
            assert!(
                lowered
                    .program
                    .instances
                    .iter()
                    .any(|(_, instance)| instance.template == function
                        && lowered
                            .program
                            .specializations
                            .instance(instance.ordinal)
                            .is_some_and(|key| !key.runtime().is_empty())),
                "{name} must run dynamically"
            );
        }
    }

    #[test]
    fn local_recursive_generic_is_selected_dynamically() {
        // A local generic `def` that calls itself captures its own binding's
        // initialization-state cell. The cell holds no function value, so the
        // capture is not the function-behind-a-reference restriction.
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let program = codegen_corpus()
            .iter()
            .find(|program| program.name == "local_recursive_generic")
            .unwrap();
        let CorpusSource::Inline(source) = program.source else {
            panic!("the regression is inline");
        };
        let loaded = ProgramLoader::new()
            .with_standard_library_root(workspace.join("stdlib"))
            .load_source(source, workspace)
            .unwrap();
        let checked = TypeChecker::new()
            .check(NameResolver::new().resolve_program(loaded).unwrap())
            .unwrap();
        let lowered = Lowerer::new()
            .with_dictionary_pass_generic_calls(true)
            .lower(&checked)
            .unwrap();
        let function = lowered
            .program
            .functions
            .iter()
            .find_map(|(_, id, function)| (function.name == "recur").then_some(id))
            .unwrap();
        assert_eq!(
            lowered.program.dynamic_support(function),
            DynamicSupport::Supported,
            "{:?}",
            lowered.program.dynamic_support_reasons.get(&function)
        );
        assert!(lowered.differential_dynamic_call_count() > 0);
    }

    #[test]
    fn audited_forms_are_selected_dynamically() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let program = codegen_corpus()
            .iter()
            .find(|program| program.name == "dynamic_audit_forms")
            .unwrap();
        let CorpusSource::Inline(source) = program.source else {
            panic!("audit regression is inline");
        };
        let loaded = ProgramLoader::new()
            .with_standard_library_root(workspace.join("stdlib"))
            .load_source(source, workspace)
            .unwrap();
        let checked = TypeChecker::new()
            .check(NameResolver::new().resolve_program(loaded).unwrap())
            .unwrap();
        let lowered = Lowerer::new()
            .with_dictionary_pass_generic_calls(true)
            .lower(&checked)
            .unwrap();
        let phantom = lowered
            .program
            .functions
            .iter()
            .find_map(|(_, id, function)| (function.name == "phantom").then_some(id))
            .unwrap();
        assert!(
            lowered
                .program
                .relevant_parameters(phantom)
                .type_parameters()
                .next()
                .is_none(),
            "unused Copy parameter must not change the target ABI"
        );
        for name in [
            "indexed",
            "named",
            "defaults",
            "spreads",
            "literals",
            "summed_literal",
            "propagate",
            "asserted",
            "nested_generic",
            "concrete_capture",
            "compound_capture",
            "function_capture",
            "native_capture",
            "native_cell",
            "packed_buffer",
            "phantom_argument",
        ] {
            let function = lowered
                .program
                .functions
                .iter()
                .find_map(|(_, id, function)| (function.name == name).then_some(id))
                .unwrap();
            assert_eq!(
                lowered.program.dynamic_support(function),
                DynamicSupport::Supported,
                "{name}"
            );
            assert!(
                lowered
                    .program
                    .instances
                    .iter()
                    .any(|(_, instance)| instance.template == function
                        && lowered
                            .program
                            .specializations
                            .instance(instance.ordinal)
                            .is_some_and(|key| !key.runtime().is_empty())),
                "{name} must run dynamically"
            );
        }
        let context = inkwell::context::Context::create();
        let ir = crate::CodeGenerator::new(&context)
            .compile_module(&lowered)
            .unwrap();
        let mut dynamic = false;
        let mut loads = 0;
        for line in ir.lines() {
            if line.starts_with("define ") {
                dynamic = line.contains("_dynamic(");
            }
            if dynamic && line.contains("buffer.pointer") && line.contains(" = load ") {
                assert!(
                    line.ends_with(&format!("align {}", std::mem::align_of::<usize>())),
                    "naturally aligned dynamic pointer load: {line}"
                );
                loads += 1;
            }
        }
        assert!(loads > 0, "dynamic storage loads must be exercised");
    }

    #[test]
    fn aggregate_reabstraction_is_selected_inside_dynamic_bodies() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let program = codegen_corpus()
            .iter()
            .find(|program| program.name == "dynamic_reabstraction_shapes")
            .unwrap();
        let CorpusSource::Inline(source) = program.source else {
            panic!("conversion regression is inline");
        };
        let loaded = ProgramLoader::new()
            .with_standard_library_root(workspace.join("stdlib"))
            .load_source(source, workspace)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(loaded).unwrap();
        let checked = TypeChecker::new().check(resolved).unwrap();
        let lowered = Lowerer::new()
            .with_dictionary_pass_generic_calls(true)
            .lower(&checked)
            .unwrap();
        for name in [
            "nested",
            "product",
            "wrapper",
            "summed",
            "array",
            "opaque",
            "mutator",
            "reset",
            "reset_box",
        ] {
            let function = lowered
                .program
                .functions
                .iter()
                .find_map(|(_, id, function)| (function.name == name).then_some(id))
                .unwrap();
            assert_eq!(
                lowered.program.dynamic_support(function),
                DynamicSupport::Supported,
                "{name}"
            );
            assert!(
                lowered
                    .program
                    .instances
                    .iter()
                    .any(|(_, instance)| instance.template == function
                        && lowered
                            .program
                            .specializations
                            .instance(instance.ordinal)
                            .is_some_and(|key| !key.runtime().is_empty())),
                "{name} must run dynamically"
            );
        }
        for direction in [
            crate::specialization::ReabstractionDirection::ConcreteToDynamic,
            crate::specialization::ReabstractionDirection::DynamicToConcrete,
        ] {
            assert!(lowered.program.artifacts.iter().any(|(_, artifact)| {
                matches!(&artifact.plan, Some(crate::LoweredArtifactPlan::ValueConversion(plan)) if plan.direction == direction)
            }), "both conversion directions must be planned");
            assert!(lowered.program.artifacts.iter().any(|(_, artifact)| {
                matches!(&artifact.plan, Some(crate::LoweredArtifactPlan::ReabstractionThunk(plan)) if plan.direction == direction && matches!(plan.concrete.parameter.as_ref(), CheckedType::Product(_)) && matches!(plan.dynamic.parameter.as_ref(), CheckedType::Parameter { .. }))
            }), "tuple parameters must adapt between flattened and opaque ABIs");
        }
    }

    #[test]
    fn mixed_layout_conversions_take_descriptors() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let program = codegen_corpus()
            .iter()
            .find(|program| program.name == "polymorphic_mixed_layout_calls")
            .unwrap();
        let CorpusSource::Inline(source) = program.source else {
            panic!("mixed-layout regression is inline");
        };
        let loaded = ProgramLoader::new()
            .with_standard_library_root(workspace.join("stdlib"))
            .load_source(source, workspace)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(loaded).unwrap();
        let checked = TypeChecker::new().check(resolved).unwrap();
        let lowered = Lowerer::new().lower(&checked).unwrap();
        // Arguments convert into the body's shape, and results and `mut`
        // writebacks back out, each through the value's descriptor.
        for direction in [
            crate::specialization::ReabstractionDirection::ConcreteToDynamic,
            crate::specialization::ReabstractionDirection::DynamicToConcrete,
        ] {
            assert!(
                lowered.program.artifacts.iter().any(|(_, artifact)| {
                    matches!(&artifact.plan, Some(crate::LoweredArtifactPlan::ValueConversion(plan))
                        if plan.direction == direction
                            && super::super::value_conversions::takes_descriptor(&plan.concrete))
                }),
                "a descriptor-taking conversion must be planned in each direction"
            );
        }
    }

    /// The tracked coverage report. Every generic template in the
    /// corpus is counted once per program; a template is in scope when any of
    /// its reasons is. Baseline increases require new corpus programs or
    /// corrected classifications and an explanation in the commit message.
    #[test]
    fn coverage_report_stays_within_baseline() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let mut reason_counts: HashMap<RejectionReason, usize> = HashMap::new();
        let mut in_scope = 0usize;
        let mut unsupported = 0usize;
        let mut total = 0usize;
        // A source span distinguishes same-named methods in different impls;
        // semantic IDs alone change when a different corpus imports modules.
        let mut trait_templates =
            std::collections::BTreeMap::<String, BTreeSet<RejectionReason>>::new();
        let mut trait_counts = std::collections::BTreeMap::<String, usize>::new();
        let mut trait_only = 0usize;
        // Templates with effect parameters, with or without type parameters.
        let mut effect_total = 0usize;
        let mut effect_reason_counts: HashMap<RejectionReason, usize> = HashMap::new();
        for (source_index, (source, root)) in corpus_sources().into_iter().enumerate() {
            if source_index % 20 == 0 {
                eprintln!("coverage: checking source {}", source_index + 1);
            }
            let loaded = ProgramLoader::new()
                .with_standard_library_root(workspace.join("stdlib"))
                .load_source(&source, &root)
                .unwrap_or_else(|diagnostics| panic!("corpus should load: {diagnostics:?}"));
            let resolved = NameResolver::new()
                .resolve_program(loaded)
                .unwrap_or_else(|diagnostics| panic!("corpus should resolve: {diagnostics:?}"));
            let checked = TypeChecker::new()
                .check(resolved)
                .unwrap_or_else(|diagnostics| panic!("corpus should check: {diagnostics:?}"));
            let lowered = match Lowerer::new()
                .with_dictionary_pass_generic_calls(true)
                .lower(&checked)
            {
                Ok(lowered) => lowered,
                Err(diagnostics) => {
                    panic!("corpus should lower with dictionary passing: {diagnostics:?}")
                }
            };
            let program = &lowered.program;
            for (_, _, function) in program.functions.iter() {
                let relevant = program.relevant_parameters(function.semantic_id);
                let has_effect = relevant.effect_parameters().next().is_some();
                if has_effect {
                    effect_total += 1;
                    for reason in program
                        .dynamic_support_reasons
                        .get(&function.semantic_id)
                        .into_iter()
                        .flatten()
                    {
                        *effect_reason_counts.entry(*reason).or_default() += 1;
                    }
                }
                if relevant.type_parameters().next().is_none() {
                    continue;
                }
                total += 1;
                let Some(reasons) = program.dynamic_support_reasons.get(&function.semantic_id)
                else {
                    continue;
                };
                unsupported += 1;
                if reasons.iter().any(|reason| {
                    matches!(
                        reason,
                        RejectionReason::TraitCallRuntime
                    )
                }) {
                    // Generated symbol names contain semantic IDs and change
                    // between imports. The body origin is the stable identity.
                    // Inline programs all load as <stdin>, so keep them separate.
                    let source_scope = match &function.body_origin.span {
                        staple_syntax::Span::User {
                            source: Some(source),
                            ..
                        } if source.ends_with("<stdin>.sta") => format!("corpus {source_index} "),
                        _ => String::new(),
                    };
                    trait_templates
                        .entry(format!("{source_scope}{:?}", function.body_origin.span))
                        .or_default()
                        .extend(reasons.iter().copied());
                    if reasons.iter().all(|reason| {
                        matches!(
                            reason,
                            RejectionReason::TraitCallRuntime
                        )
                    }) {
                        trait_only += 1;
                    }
                    let mut traits = BTreeSet::new();
                    for bound in &function.bounds {
                        if Some(bound.trait_id) == program.semantic_ids.copy_trait
                            || Some(bound.trait_id) == program.semantic_ids.sized_trait
                        {
                            continue;
                        }
                        if let Some(metadata) = program.traits.get(bound.trait_id) {
                            traits.insert(metadata.name.clone());
                        }
                    }
                    for name in traits {
                        *trait_counts.entry(name).or_default() += 1;
                    }
                }
                for reason in reasons {
                    *reason_counts.entry(*reason).or_default() += 1;
                }
                if program
                    .dynamic_support_in_scope
                    .get(&function.semantic_id)
                    .copied()
                    .unwrap_or(false)
                {
                    in_scope += 1;
                    eprintln!("in-scope template `{}`: {:?}", function.name, reasons);
                }
            }
        }
        let mut rows = reason_counts.into_iter().collect::<Vec<_>>();
        rows.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        eprintln!(
            "generic templates: {total}, unsupported: {unsupported}, in scope: {in_scope} (baseline {IN_SCOPE_BASELINE})"
        );
        for (reason, count) in rows {
            eprintln!(
                "{count:5}  {}{}",
                reason.description(),
                if reason.in_scope() { "" } else { "  (allowed)" }
            );
        }
        eprintln!(
            "trait-only template occurrences: {trait_only}; distinct trait-rejected templates: {}",
            trait_templates.len()
        );
        for (name, count) in trait_counts {
            eprintln!("trait {name}: {count} bounded template occurrences");
        }
        for (name, reasons) in trait_templates {
            eprintln!("trait template {name}: {reasons:?}");
        }
        let effect_unsupported = effect_reason_counts.values().sum::<usize>();
        eprintln!(
            "effect-generic templates: {effect_total}, rejection reasons: {effect_unsupported}"
        );
        let mut effect_rows = effect_reason_counts.into_iter().collect::<Vec<_>>();
        effect_rows.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        for (reason, count) in effect_rows {
            eprintln!("{count:5}  {}", reason.description());
        }
        assert!(
            effect_total > 0,
            "the corpus must contain effect-generic templates"
        );
        assert!(
            in_scope <= IN_SCOPE_BASELINE,
            "in-scope rejections rose to {in_scope}, above the {IN_SCOPE_BASELINE} baseline"
        );
    }
}

/// Every type parameter id a type mentions, including inside schemes and
/// effect rows' resource types.
fn collect_parameter_ids(ty: &CheckedType, ids: &mut HashSet<crate::TypeParameterId>) {
    match ty {
        CheckedType::Parameter { id, .. } => {
            ids.insert(*id);
        }
        CheckedType::Forall(scheme) => {
            collect_parameter_ids(&scheme.body, ids);
            for bound in &scheme.trait_bounds {
                for argument in &bound.arguments {
                    collect_parameter_ids(argument, ids);
                }
            }
        }
        CheckedType::Function(function) => {
            collect_parameter_ids(&function.parameter, ids);
            collect_parameter_ids(&function.result, ids);
        }
        CheckedType::Product(product) => {
            for element in &product.elements {
                collect_parameter_ids(&element.value_type, ids);
            }
        }
        CheckedType::Sum(sum) => {
            for alternative in &sum.alternatives {
                collect_parameter_ids(alternative, ids);
            }
        }
        CheckedType::Wrapper { representation, .. } => collect_parameter_ids(representation, ids),
        CheckedType::Ref(inner) | CheckedType::Slice(inner) | CheckedType::Buffer(inner) => {
            collect_parameter_ids(inner, ids)
        }
        CheckedType::Opaque { arguments, .. } | CheckedType::TypeConstructor { arguments, .. } => {
            for argument in arguments {
                collect_parameter_ids(argument, ids);
            }
        }
        CheckedType::Array { element, .. } => collect_parameter_ids(element, ids),
        _ => {}
    }
}
