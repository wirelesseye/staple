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
    BlockId, ExpressionId, ItemId, LoweredAccessKind, LoweredCallableTarget,
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

    /// Computes the greatest fixed point of per-template dynamic support.
    pub(crate) fn compute_dynamic_support(&mut self) {
        let functions = self
            .functions
            .iter()
            .map(|(_, id, _)| id)
            .collect::<Vec<_>>();
        let mut support = functions
            .iter()
            .map(|function| (*function, DynamicSupport::Supported))
            .collect::<HashMap<_, _>>();
        let mut dependencies = HashMap::new();
        for function in &functions {
            let (scan, deps) = self.scan_dynamic_support(*function);
            support.insert(*function, scan);
            dependencies.insert(*function, deps);
        }
        loop {
            let mut changed = false;
            for function in &functions {
                if !support[function].is_supported() {
                    continue;
                }
                let dependent = dependencies[function]
                    .iter()
                    .any(|dependency| !support.get(dependency).copied().unwrap_or(DynamicSupport::Supported).is_supported());
                if dependent {
                    support.insert(*function, DynamicSupport::Unsupported);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        self.dynamic_support = support;
    }

    /// One template's immediate support and the generic callees its dynamic
    /// implementation would call with run-time arguments.
    fn scan_dynamic_support(
        &self,
        function: FunctionId,
    ) -> (DynamicSupport, HashSet<FunctionId>) {
        let mut walker = SupportWalker {
            program: self,
            unsupported: false,
            dependencies: HashSet::new(),
            visited_blocks: HashSet::new(),
            visited_items: HashSet::new(),
            visited_expressions: HashSet::new(),
            visited_patterns: HashSet::new(),
            visited_places: HashSet::new(),
            visited_calls: HashSet::new(),
        };
        let Some(template) = self.functions.get(function) else {
            return (DynamicSupport::Unsupported, HashSet::new());
        };
        if !template.captures.is_empty()
            || template.signature.effects.variable.is_some()
            || !template.signature.effects.resources.is_empty()
            || contains_parameter_product(&CheckedType::Function(template.signature.clone()))
            || contains_polymorphic_function(&template.signature.parameter)
            || contains_polymorphic_function(&template.signature.result)
        {
            walker.unsupported = true;
        }
        let copy = self.semantic_ids.copy_trait;
        let sized = self.semantic_ids.sized_trait;
        for bound in &template.bounds {
            if Some(bound.trait_id) != copy && Some(bound.trait_id) != sized {
                walker.unsupported = true;
            }
        }
        if let Some(body) = template.body {
            walker.block(body);
        }
        let support = if walker.unsupported {
            DynamicSupport::Unsupported
        } else {
            DynamicSupport::Supported
        };
        (support, walker.dependencies)
    }
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
    unsupported: bool,
    dependencies: HashSet<FunctionId>,
    visited_blocks: HashSet<BlockId>,
    visited_items: HashSet<ItemId>,
    visited_expressions: HashSet<ExpressionId>,
    visited_patterns: HashSet<PatternId>,
    visited_places: HashSet<PlaceId>,
    visited_calls: HashSet<crate::lower::LoweredCallId>,
}

impl SupportWalker<'_> {
    fn block(&mut self, id: BlockId) {
        if !self.visited_blocks.insert(id) {
            return;
        }
        let Some(block) = self.program.blocks.get(id) else {
            self.unsupported = true;
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
            self.unsupported = true;
            return;
        };
        match &item.kind {
            LoweredItemKind::Binding(binding) => {
                if binding.signal || binding.derived || binding.generic || binding.reactive.is_some() {
                    self.unsupported = true;
                }
                if let Some(value) = binding.value {
                    self.expression(value);
                }
            }
            LoweredItemKind::PatternBinding(binding) => {
                if binding.propagating || binding.asserting {
                    self.unsupported = true;
                }
                self.expression(binding.value);
            }
            LoweredItemKind::Assignment(assignment) => {
                if assignment.mutate_index.is_some() || assignment.signal_notify.is_some() {
                    self.unsupported = true;
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
            self.unsupported = true;
            return;
        };
        if contains_polymorphic_function(&expression.value_type) {
            self.unsupported = true;
        }
        if let Some(coercion) = &expression.coercion
            && (contains_polymorphic_function(&coercion.source)
                || contains_polymorphic_function(&coercion.target))
        {
            self.unsupported = true;
        }
        match &expression.kind {
            LoweredExpressionKind::Deferred(_)
            | LoweredExpressionKind::String(_)
            | LoweredExpressionKind::CString(_)
            | LoweredExpressionKind::StringTemplate(_)
            | LoweredExpressionKind::Index(_)
            | LoweredExpressionKind::CallableValue(_)
            | LoweredExpressionKind::Resource(_)
            | LoweredExpressionKind::With(_)
            | LoweredExpressionKind::Coro(_)
            | LoweredExpressionKind::Await(_) => self.unsupported = true,
            LoweredExpressionKind::Block(block) => self.block(*block),
            LoweredExpressionKind::Name(name) => {
                if name.reactive.is_some() {
                    self.unsupported = true;
                }
            }
            LoweredExpressionKind::Integer(_) | LoweredExpressionKind::Float(_) => {}
            LoweredExpressionKind::Access(access) => {
                let supported = match &access.kind {
                    LoweredAccessKind::Product { dereference, .. }
                    | LoweredAccessKind::Scalar { dereference } => dereference.is_empty(),
                    LoweredAccessKind::Representation { .. } | LoweredAccessKind::Slice { .. } => {
                        false
                    }
                };
                if !supported {
                    self.unsupported = true;
                }
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
                        | LoweredProductStep::NamedSpread { .. } => self.unsupported = true,
                    }
                }
                for field in &product.fields {
                    self.expression(*field);
                }
            }
            LoweredExpressionKind::RepeatedProduct(product) => {
                if matches!(product.count, crate::lower::LoweredRepeatCount::Symbolic(_)) {
                    self.unsupported = true;
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
            self.unsupported = true;
            return;
        };
        match &pattern.kind {
            LoweredPatternKind::Literal { .. } => self.unsupported = true,
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
                    self.unsupported = true;
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
            self.unsupported = true;
            return;
        };
        match &place.kind {
            crate::lower::LoweredPlaceKind::Symbol { .. }
            | crate::lower::LoweredPlaceKind::CapturedCell { .. } => {}
            crate::lower::LoweredPlaceKind::Temporary { expression } => self.expression(*expression),
            crate::lower::LoweredPlaceKind::Resource { .. } => self.unsupported = true,
            crate::lower::LoweredPlaceKind::Dereference { reference, .. } => {
                self.expression(*reference)
            }
            crate::lower::LoweredPlaceKind::ProductElement { base, slice, .. } => {
                if *slice {
                    self.unsupported = true;
                }
                self.place(*base);
            }
            crate::lower::LoweredPlaceKind::Representation { base } => self.place(*base),
            crate::lower::LoweredPlaceKind::Indexed { base, index } => {
                self.unsupported = true;
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
            self.unsupported = true;
            return;
        };
        if call.reactive.is_some() || !call.resource_bindings.is_empty() {
            self.unsupported = true;
        }
        if contains_polymorphic_function(&CheckedType::Function(call.function_type.clone()))
            || call
                .substitutions
                .types
                .iter()
                .any(|entry| contains_polymorphic_function(&entry.value_type))
        {
            self.unsupported = true;
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
                        self.unsupported = true;
                    }
                }
            }
            LoweredCallableTarget::Constructor { recursive, .. } => {
                if recursive.is_some() {
                    self.unsupported = true;
                }
            }
            // A concrete trait call dispatches to a monomorphized implementation
            // and needs no dictionary; a parameter-dependent one is Stage 6.
            LoweredCallableTarget::TraitImplementation { .. } => {
                if mentions_parameters {
                    self.unsupported = true;
                }
            }
            LoweredCallableTarget::StructuralTraitMethod { .. }
            | LoweredCallableTarget::IndirectClosure { .. }
            | LoweredCallableTarget::ExternalFunction { .. }
            | LoweredCallableTarget::Intrinsic { .. } => self.unsupported = true,
        }
    }

    fn argument(&mut self, call: &crate::lower::LoweredCall, index: usize) {
        let Some(argument) = call.arguments.get(index) else {
            return;
        };
        if argument.thunk.is_some() {
            self.unsupported = true;
        }
        if let Some(expression) = argument.expression {
            self.expression(expression);
        }
        if let Some(place) = argument.place {
            self.place(place);
        }
    }
}
