//! Transitive support analysis for the hidden differential lowering option.
//!
//! Decision 11 lowers direct generic calls from concrete owners through
//! dynamic implementations. A dynamic implementation is only selected when
//! the callee's body, and every generic callee it would call with run-time
//! type arguments, can be emitted by the dynamic emitter. This module computes
//! that greatest fixed point over lowered templates: a function starts
//! supported and is excluded when its body uses a construct the dynamic
//! emitter does not handle, or when it depends on an excluded function.
//!
//! The same analysis produces the transitive restriction reports Stage 4b's
//! gate needs; until then it only gates the option.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::lower::{
    BlockId, ExpressionId, ItemId, LoweredCallableTarget,
    LoweredExpressionKind, LoweredItemKind, LoweredPatternKind, LoweredProductStep, LoweredProgram,
    PatternId, PlaceId,
};
use crate::{CheckedType, FunctionId, TypeParameterId, contains_type_parameter};

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
/// Every reason is classified against R0.1's allowed list: a template is
/// in scope for 4a when it has at least one in-scope reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum RejectionReason {
    /// A callable value expression (`CallableValue`).
    CallableValue,
    /// A value whose type mentions a function type with run-time parameters.
    FunctionTypedValue,
    /// A coercion whose source or target mentions such a function type.
    FunctionTypedCoercion,
    /// A signature parameter or result is such a function type.
    FunctionTypedSignature,
    /// A call boundary passes or returns such a function type.
    FunctionTypeAtCallBoundary,
    /// A call through a closure value.
    IndirectClosureCall,
    /// A closure body captures values.
    Captures,
    /// A parameter product appears in a dynamic signature.
    ParameterProducts,
    /// An intrinsic outside the supported and allowed sets.
    Intrinsic,
    /// Buffer operations other than `__buffer_clone` (R6).
    BufferIntrinsic,
    /// String and CString operations (R7).
    StringIntrinsic,
    /// A call with hidden resource bindings.
    ResourceCall,
    /// A resource expression in the body.
    ResourceExpression,
    /// A resource mutation target.
    ResourcePlace,
    /// A `with` provider scope.
    WithExpression,
    /// A signature declares resource effects.
    ResourceEffects,
    /// A string template.
    StringTemplate,
    /// An index expression (trait-dispatched).
    IndexExpression,
    /// An implicit thunk argument.
    ImplicitThunkArgument,
    /// A propagating or asserting pattern binding.
    PropagatingBinding,
    /// A nested generic `def` binding.
    GenericBinding,
    /// A string literal pattern.
    StringPattern,
    /// A non-string literal pattern.
    LiteralPattern,
    /// A product spread step.
    ProductSpread,
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
    // Allowed reasons (belonging to later stages or documented restrictions).
    /// A trait bound other than `Copy`/`Sized` (Stage 6).
    TraitBound,
    /// A trait call with run-time arguments (Stage 6).
    TraitCallRuntime,
    /// A structural trait method dispatch (Stage 6).
    StructuralTraitDispatch,
    /// An indexed mutation (Stage 6).
    IndexedMutation,
    /// A slice mutation (Stage 6).
    SliceMutation,
    /// An effect variable in the signature (Stage 5).
    EffectVariable,
    /// An async intrinsic: completion, task, resolver (Stage 8).
    AsyncIntrinsic,
    /// A reactive intrinsic (Stage 8).
    ReactiveIntrinsic,
    /// A coroutine or `await` expression (Stage 8).
    CoroutineExpression,
    /// A reactive name read (Stage 8).
    ReactiveNameRead,
    /// A call with a reactive operation (Stage 8).
    ReactiveCall,
    /// A signal or derived binding (Stage 8).
    SignalBinding,
    /// A reactive binding record (Stage 8).
    ReactiveBinding,
    /// A signal assignment (Stage 8).
    SignalAssignment,
    /// `__buffer_clone` (Stage 6).
    BufferClone,
    /// A run-time `Natural` count (a restriction).
    RuntimeNaturalCount,
    /// `__slice_from_ref` (a restriction).
    SliceFromRef,
    /// A function value behind a reference, slice, buffer, or shared cell
    /// (a restriction). Added by R5.
    #[allow(dead_code)]
    FunctionBehindReference,
}

impl RejectionReason {
    /// Whether this reason is in scope for Stage 4a (R0.1). In-scope reasons
    /// must eventually reach zero; allowed reasons belong to a later stage or
    /// to a documented restriction.
    pub(crate) fn in_scope(self) -> bool {
        !matches!(
            self,
            RejectionReason::TraitBound
                | RejectionReason::TraitCallRuntime
                | RejectionReason::StructuralTraitDispatch
                | RejectionReason::IndexedMutation
                | RejectionReason::SliceMutation
                | RejectionReason::EffectVariable
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
            RejectionReason::FunctionTypedValue => "value of a function type",
            RejectionReason::FunctionTypedCoercion => "function-typed coercion",
            RejectionReason::FunctionTypedSignature => "function-typed signature",
            RejectionReason::FunctionTypeAtCallBoundary => "function type at a call boundary",
            RejectionReason::IndirectClosureCall => "indirect closure call",
            RejectionReason::Captures => "captures",
            RejectionReason::ParameterProducts => "parameter products",
            RejectionReason::Intrinsic => "unsupported intrinsic",
            RejectionReason::BufferIntrinsic => "buffer intrinsic",
            RejectionReason::StringIntrinsic => "string intrinsic",
            RejectionReason::ResourceCall => "resource call",
            RejectionReason::ResourceExpression => "resource expression",
            RejectionReason::ResourcePlace => "resource place",
            RejectionReason::WithExpression => "with scope",
            RejectionReason::ResourceEffects => "resource effects in the signature",
            RejectionReason::StringTemplate => "string template",
            RejectionReason::IndexExpression => "index expression",
            RejectionReason::ImplicitThunkArgument => "implicit thunk argument",
            RejectionReason::PropagatingBinding => "propagating binding",
            RejectionReason::GenericBinding => "generic binding",
            RejectionReason::StringPattern => "string pattern",
            RejectionReason::LiteralPattern => "literal pattern",
            RejectionReason::ProductSpread => "product spread",
            RejectionReason::RecursiveConstruction => "recursive construction",
            RejectionReason::Deferred => "deferred expression",
            RejectionReason::ParameterDependentNonGeneric => {
                "parameter-dependent non-generic call"
            }
            RejectionReason::ExternWithParameters => "parameter-dependent extern call",
            RejectionReason::Malformed => "malformed lowered record",
            RejectionReason::CallsExcludedTemplate => "calls an excluded template",
            RejectionReason::TraitBound => "trait bound other than Copy/Sized",
            RejectionReason::TraitCallRuntime => "trait call with run-time arguments",
            RejectionReason::StructuralTraitDispatch => "structural trait dispatch",
            RejectionReason::IndexedMutation => "indexed mutation",
            RejectionReason::SliceMutation => "slice mutation",
            RejectionReason::EffectVariable => "effect variable",
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
        IntrinsicFunction::BufferWithCapacity
        | IntrinsicFunction::BufferPush
        | IntrinsicFunction::BufferPop
        | IntrinsicFunction::BufferGet
        | IntrinsicFunction::BufferFreeze
        | IntrinsicFunction::BufferTransfer => RejectionReason::BufferIntrinsic,
        IntrinsicFunction::ParseNumber { .. }
        | IntrinsicFunction::ValidateUtf8
        | IntrinsicFunction::CStringBytes
        | IntrinsicFunction::StringHasNul
        | IntrinsicFunction::ToString { .. }
        | IntrinsicFunction::StringFromCString
        | IntrinsicFunction::StringToCString
        | IntrinsicFunction::StringAdd => RejectionReason::StringIntrinsic,
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
        _ => RejectionReason::Intrinsic,
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
    ) -> Option<BTreeSet<TypeParameterId>> {
        if !self.dictionary_pass_generic_calls || current || !caller_runtime.is_empty() {
            return None;
        }
        if !self.dynamic_support(function).is_supported() {
            return None;
        }
        let relevant = self.relevant_parameters(function);
        let runtime = relevant
            .type_parameters()
            .collect::<BTreeSet<TypeParameterId>>();
        if runtime.is_empty() {
            return None;
        }
        Some(runtime)
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

    /// One template's direct reasons and the generic callees its dynamic
    /// implementation would call with run-time arguments.
    fn scan_dynamic_support(
        &self,
        function: FunctionId,
    ) -> (BTreeSet<RejectionReason>, HashSet<FunctionId>) {
        let mut walker = SupportWalker {
            program: self,
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
        if !template.captures.is_empty() {
            walker.reject(RejectionReason::Captures);
        }
        if template.signature.effects.variable.is_some() {
            walker.reject(RejectionReason::EffectVariable);
        }
        if !template.signature.effects.resources.is_empty() {
            walker.reject(RejectionReason::ResourceEffects);
        }
        if contains_parameter_product(&CheckedType::Function(template.signature.clone())) {
            walker.reject(RejectionReason::ParameterProducts);
        }
        if contains_nested_polymorphic_function(&template.signature.parameter)
            || contains_nested_polymorphic_function(&template.signature.result)
        {
            walker.reject(RejectionReason::FunctionTypedSignature);
        }
        let copy = self.semantic_ids.copy_trait;
        let sized = self.semantic_ids.sized_trait;
        for bound in &template.bounds {
            if Some(bound.trait_id) != copy && Some(bound.trait_id) != sized {
                walker.reject(RejectionReason::TraitBound);
            }
        }
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
        IntrinsicFunction::NumericConvert { .. }
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
    )
}

/// Whether a type mentions a function type that still contains a run-time
/// parameter. Reabstraction between the concrete and dynamic ABIs is a later
/// 4a step, so such functions are excluded from the option until then.
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

/// Like [`contains_polymorphic_function`] but a top-level function type is
/// treated as the container to inspect rather than as a nested function value.
/// Used for callable signatures, where a parameter or result that is itself a
/// function value still needs reabstraction.
fn contains_nested_polymorphic_function(ty: &CheckedType) -> bool {
    match ty {
        CheckedType::Function(function) => {
            contains_polymorphic_function(&function.parameter)
                || contains_polymorphic_function(&function.result)
                || function
                    .effects
                    .resources
                    .iter()
                    .any(|resource| contains_polymorphic_function(&resource.value_type))
        }
        CheckedType::CPointer { pointee } => contains_nested_polymorphic_function(pointee),
        CheckedType::Ref(value) | CheckedType::Slice(value) | CheckedType::Buffer(value) => {
            contains_nested_polymorphic_function(value)
        }
        CheckedType::Array { element, count } => {
            contains_nested_polymorphic_function(element)
                || contains_nested_polymorphic_function(count)
        }
        CheckedType::Opaque { arguments, .. } | CheckedType::TypeConstructor { arguments, .. } => {
            arguments.iter().any(contains_nested_polymorphic_function)
        }
        CheckedType::Product(product) => product
            .elements
            .iter()
            .any(|field| contains_nested_polymorphic_function(&field.value_type)),
        CheckedType::Sum(sum) => sum
            .alternatives
            .iter()
            .any(contains_nested_polymorphic_function),
        CheckedType::Wrapper {
            arguments,
            representation,
            ..
        } => {
            arguments.iter().any(contains_nested_polymorphic_function)
                || contains_nested_polymorphic_function(representation)
        }
        _ => false,
    }
}

/// Whether a type contains a parameter product anywhere. Capable parameters
/// cannot appear in a scheme used as a value, and the dynamic emitter does not
/// model their arity.
fn contains_parameter_product(ty: &CheckedType) -> bool {
    match ty {
        CheckedType::ParameterProduct(_) => true,
        CheckedType::CPointer { pointee } => contains_parameter_product(pointee),
        CheckedType::Ref(value) | CheckedType::Slice(value) | CheckedType::Buffer(value) => {
            contains_parameter_product(value)
        }
        CheckedType::Array { element, count } => {
            contains_parameter_product(element) || contains_parameter_product(count)
        }
        CheckedType::Opaque { arguments, .. } | CheckedType::TypeConstructor { arguments, .. } => {
            arguments.iter().any(contains_parameter_product)
        }
        CheckedType::Product(product) => product
            .elements
            .iter()
            .any(|field| contains_parameter_product(&field.value_type)),
        CheckedType::Sum(sum) => sum.alternatives.iter().any(contains_parameter_product),
        CheckedType::Function(function) => {
            contains_parameter_product(&function.parameter)
                || contains_parameter_product(&function.result)
                || function
                    .effects
                    .resources
                    .iter()
                    .any(|resource| contains_parameter_product(&resource.value_type))
        }
        CheckedType::Wrapper {
            arguments,
            representation,
            ..
        } => {
            arguments.iter().any(contains_parameter_product)
                || contains_parameter_product(representation)
        }
        _ => false,
    }
}

struct SupportWalker<'program> {
    program: &'program LoweredProgram,
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
                if binding.generic {
                    self.reject(RejectionReason::GenericBinding);
                }
                if binding.reactive.is_some() {
                    self.reject(RejectionReason::ReactiveBinding);
                }
                if let Some(value) = binding.value {
                    self.expression(value);
                }
            }
            LoweredItemKind::PatternBinding(binding) => {
                if binding.propagating || binding.asserting {
                    self.reject(RejectionReason::PropagatingBinding);
                }
                self.expression(binding.value);
            }
            LoweredItemKind::Assignment(assignment) => {
                if assignment.mutate_index.is_some() {
                    self.reject(RejectionReason::IndexedMutation);
                }
                if assignment.signal_notify.is_some() {
                    self.reject(RejectionReason::SignalAssignment);
                }
                self.place(assignment.target);
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
        if contains_polymorphic_function(&expression.value_type) {
            self.reject(RejectionReason::FunctionTypedValue);
        }
        if let Some(coercion) = &expression.coercion
            && (contains_polymorphic_function(&coercion.source)
                || contains_polymorphic_function(&coercion.target))
        {
            self.reject(RejectionReason::FunctionTypedCoercion);
        }
        match &expression.kind {
            LoweredExpressionKind::String(_) | LoweredExpressionKind::CString(_) => {}
            LoweredExpressionKind::Deferred(_) => self.reject(RejectionReason::Deferred),
            LoweredExpressionKind::StringTemplate(_) => {
                self.reject(RejectionReason::StringTemplate)
            }
            LoweredExpressionKind::Index(_) => self.reject(RejectionReason::IndexExpression),
            LoweredExpressionKind::CallableValue(_) => {
                self.reject(RejectionReason::CallableValue)
            }
            LoweredExpressionKind::Resource(_) => {
                self.reject(RejectionReason::ResourceExpression)
            }
            LoweredExpressionKind::With(_) => self.reject(RejectionReason::WithExpression),
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
                        LoweredProductStep::PositionalSpread { .. }
                        | LoweredProductStep::NamedSpread { .. } => self.reject(RejectionReason::ProductSpread),
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
            LoweredPatternKind::Literal { .. } => self.reject(RejectionReason::LiteralPattern),
            LoweredPatternKind::Wildcard | LoweredPatternKind::Binding { .. } => {}
            LoweredPatternKind::Product { elements, .. } => {
                for element in elements {
                    self.pattern(*element);
                }
            }
            LoweredPatternKind::Nominal { argument, .. } => {
                if matches!(
                    &pattern.test.subject,
                    CheckedType::String | CheckedType::StringLiteralSet(_)
                ) {
                    self.reject(RejectionReason::StringPattern);
                }
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
            crate::lower::LoweredPlaceKind::Temporary { expression } => self.expression(*expression),
            crate::lower::LoweredPlaceKind::Resource { .. } => self.reject(RejectionReason::ResourcePlace),
            crate::lower::LoweredPlaceKind::Dereference { reference, .. } => {
                self.expression(*reference)
            }
            crate::lower::LoweredPlaceKind::ProductElement { base, slice, .. } => {
                if *slice {
                    self.reject(RejectionReason::SliceMutation);
                }
                self.place(*base);
            }
            crate::lower::LoweredPlaceKind::Representation { base } => self.place(*base),
            crate::lower::LoweredPlaceKind::Indexed { base, index } => {
                self.reject(RejectionReason::IndexedMutation);
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
        if !call.resource_bindings.is_empty() {
            self.reject(RejectionReason::ResourceCall);
        }
        if call.reactive.is_some() {
            self.reject(RejectionReason::ReactiveCall);
        }
        if contains_nested_polymorphic_function(&call.function_type.parameter)
            || contains_nested_polymorphic_function(&call.function_type.result)
            || call
                .substitutions
                .types
                .iter()
                .any(|entry| contains_polymorphic_function(&entry.value_type))
        {
            self.reject(RejectionReason::FunctionTypeAtCallBoundary);
        }
        for step in &call.steps {
            match step {
                crate::lower::LoweredCallStep::Callee { expression } => self.expression(*expression),
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
        let mentions_parameters = contains_type_parameter(&CheckedType::Function(
            call.function_type.clone(),
        )) || call
            .substitutions
            .types
            .iter()
            .any(|entry| contains_type_parameter(&entry.value_type));
        match &call.target {
            LoweredCallableTarget::DirectFunction { function, .. } => {
                if mentions_parameters {
                    let generic = self
                        .program
                        .relevant_parameters(*function)
                        .type_parameters()
                        .next()
                        .is_some();
                    if generic {
                        self.dependencies.insert(*function);
                    } else {
                        self.reject(RejectionReason::ParameterDependentNonGeneric);
                    }
                }
            }
            LoweredCallableTarget::Constructor { recursive, .. } => {
                if recursive.is_some() {
                    self.reject(RejectionReason::RecursiveConstruction);
                }
            }
            // A concrete trait call dispatches to a monomorphized implementation
            // and needs no dictionary; a parameter-dependent one is Stage 6.
            LoweredCallableTarget::TraitImplementation { .. } => {
                if mentions_parameters {
                    self.reject(RejectionReason::TraitCallRuntime);
                }
            }
            LoweredCallableTarget::StructuralTraitMethod { .. } => {
                self.reject(RejectionReason::StructuralTraitDispatch)
            }
            LoweredCallableTarget::IndirectClosure { .. } => self.reject(RejectionReason::IndirectClosureCall),
            LoweredCallableTarget::ExternalFunction { .. } => {
                if mentions_parameters {
                    self.reject(RejectionReason::ExternWithParameters);
                }
            }
            LoweredCallableTarget::Intrinsic { intrinsic, .. } => {
                if !dynamic_scalar_intrinsic(*intrinsic) {
                    self.reject(intrinsic_rejection(*intrinsic));
                }
            }
        }
    }

    fn argument(&mut self, call: &crate::lower::LoweredCall, index: usize) {
        let Some(argument) = call.arguments.get(index) else {
            return;
        };
        if argument.thunk.is_some() {
            self.reject(RejectionReason::ImplicitThunkArgument);
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
    /// Each remainder-plan step lowers it in the same commit; R8 sets it to 0.
    const IN_SCOPE_BASELINE: usize = 3047;

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

    /// The tracked coverage report (R0.2). Every generic template in the
    /// corpus is counted once per program; a template is in scope when any of
    /// its reasons is. The count may only go down.
    #[test]
    fn coverage_report_stays_within_baseline() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let mut reason_counts: HashMap<RejectionReason, usize> = HashMap::new();
        let mut in_scope = 0usize;
        let mut unsupported = 0usize;
        let mut total = 0usize;
        for (source, root) in corpus_sources() {
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
                Err(_) => continue,
            };
            let program = &lowered.program;
            for (_, _, function) in program.functions.iter() {
                if program
                    .relevant_parameters(function.semantic_id)
                    .type_parameters()
                    .next()
                    .is_none()
                {
                    continue;
                }
                total += 1;
                let Some(reasons) = program.dynamic_support_reasons.get(&function.semantic_id)
                else {
                    continue;
                };
                unsupported += 1;
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
        assert!(
            in_scope <= IN_SCOPE_BASELINE,
            "in-scope rejections rose to {in_scope}, above the {IN_SCOPE_BASELINE} baseline"
        );
    }
}
