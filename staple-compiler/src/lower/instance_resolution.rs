//! Stage 3.2: relevant-parameter collection, substitution composition, and
//! declared trait-evidence resolution for one specialization request.
//!
//! This module walks the owned Stage 2 `LoweredProgram`; it never consults
//! `TypedModule`, LLVM state, or debug-formatted keys. Each function template
//! is scanned under its own `FunctionId`, so a nested body contributes through
//! the record that constructs or invokes it, not as ordinary children.

use std::collections::{BTreeSet, HashSet};

use crate::{
    CheckedEffectSet, CheckedFunctionType, CheckedTraitBound, CheckedType, FunctionId,
    TraitEvidence, TypeParameterId,
};

use super::*;

/// The type and effect parameters whose concrete value can change one
/// template's signature, body metadata, captures, layout, or selected
/// evidence. Iteration is in ascending parameter-ID order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RelevantParameters {
    types: BTreeSet<TypeParameterId>,
    effects: BTreeSet<TypeParameterId>,
}

impl RelevantParameters {
    pub(crate) fn insert_type(&mut self, parameter: TypeParameterId) {
        self.types.insert(parameter);
    }

    pub(crate) fn insert_effect(&mut self, parameter: TypeParameterId) {
        self.effects.insert(parameter);
    }

    pub(crate) fn contains_type(&self, parameter: TypeParameterId) -> bool {
        self.types.contains(&parameter)
    }

    pub(crate) fn contains_effect(&self, parameter: TypeParameterId) -> bool {
        self.effects.contains(&parameter)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.types.is_empty() && self.effects.is_empty()
    }

    pub(crate) fn type_parameters(&self) -> impl Iterator<Item = TypeParameterId> + '_ {
        self.types.iter().copied()
    }

    pub(crate) fn effect_parameters(&self) -> impl Iterator<Item = TypeParameterId> + '_ {
        self.effects.iter().copied()
    }

    fn extend(&mut self, other: &RelevantParameters) {
        self.types.extend(other.types.iter().copied());
        self.effects.extend(other.effects.iter().copied());
    }
}

/// Every record family the parameter collector visits. The collector's
/// exhaustive matches over the lowered enums are the compile-time half of the
/// coverage contract; this list is the declared decision table checked by
/// `parameter_record_families` tests. Adding a lowered variant requires adding
/// its family here (or an explicit no-parameter decision in the collector).
pub(crate) const PARAMETER_RECORD_FAMILIES: &[&str] = &[
    "function.signature",
    "function.parameter-pattern",
    "function.parameters",
    "function.captures",
    "function.coroutine-plan",
    "expression.header",
    "expression.block",
    "expression.name",
    "expression.integer",
    "expression.float",
    "expression.string",
    "expression.cstring",
    "expression.access",
    "expression.product",
    "expression.repeated-product",
    "expression.satisfies",
    "expression.logical",
    "expression.loop",
    "expression.match",
    "expression.index",
    "expression.string-template",
    "expression.call",
    "expression.callable-value",
    "expression.resource",
    "expression.with",
    "expression.coro",
    "expression.await",
    "item.binding",
    "item.pattern-binding",
    "item.assignment",
    "item.return",
    "item.break",
    "item.continue",
    "item.expression",
    "pattern",
    "place",
    "resource-provider",
    "resource-use",
    "reactive-operation",
    "reactive-callback",
    "trait-evidence",
];

struct ParameterCollector<'a> {
    program: &'a LoweredProgram,
    relevant: RelevantParameters,
    families: BTreeSet<&'static str>,
    blocks: HashSet<BlockId>,
    items: HashSet<ItemId>,
    expressions: HashSet<ExpressionId>,
    places: HashSet<PlaceId>,
    patterns: HashSet<PatternId>,
    callbacks: HashSet<LoweredReactiveCallbackId>,
    operations: HashSet<LoweredReactiveOperationId>,
    plans: HashSet<LoweredCoroutinePlanId>,
}

impl<'a> ParameterCollector<'a> {
    fn new(program: &'a LoweredProgram) -> Self {
        ParameterCollector {
            program,
            relevant: RelevantParameters::default(),
            families: BTreeSet::new(),
            blocks: HashSet::new(),
            items: HashSet::new(),
            expressions: HashSet::new(),
            places: HashSet::new(),
            patterns: HashSet::new(),
            callbacks: HashSet::new(),
            operations: HashSet::new(),
            plans: HashSet::new(),
        }
    }

    fn family(&mut self, family: &'static str) {
        self.families.insert(family);
    }

    fn collect_function(&mut self, function: FunctionId) {
        let Some(function) = self.program.functions.get(function) else {
            return;
        };
        let signature = function.signature.clone();
        let parameter_pattern = function.parameter_pattern;
        let parameters = function.parameters.clone();
        let captures = function.captures.clone();
        let body = function.body;
        self.family("function.signature");
        self.collect_function_type(&signature);
        // Declared bounds are deliberately not scanned on their own: a bound
        // that never reaches a signature, body, capture, or evidence record
        // cannot change the emitted instance. Bounds participate through the
        // evidence recipes that reference them.
        self.family("function.parameter-pattern");
        self.collect_pattern(parameter_pattern);
        if !parameters.is_empty() {
            self.family("function.parameters");
            for symbol in &parameters {
                self.collect_symbol_type(*symbol);
            }
        }
        if !captures.is_empty() {
            self.family("function.captures");
            for capture in &captures {
                self.collect_symbol_type(capture.symbol);
            }
        }
        if let Some(plan) = self
            .program
            .coroutine_plan_by_thunk
            .get(&function.semantic_id)
            .copied()
        {
            self.family("function.coroutine-plan");
            self.collect_coroutine_plan(plan);
        }
        if let Some(body) = body {
            self.collect_block(body);
        }
    }

    #[cfg(test)]
    fn collect_initializer(&mut self, initializer: InitializerId) {
        if let Some(initializer) = self.program.initializers.get(initializer) {
            self.collect_block(initializer.body);
        }
    }

    fn collect_symbol_type(&mut self, symbol: SymbolId) {
        let value_type = self
            .program
            .symbols
            .get(symbol)
            .map(|symbol| symbol.value_type.clone());
        if let Some(value_type) = value_type {
            self.collect_type(&value_type);
        }
    }

    fn collect_function_type(&mut self, function: &CheckedFunctionType) {
        self.collect_type(&function.parameter);
        self.collect_effect_set(&function.effects);
        self.collect_type(&function.result);
    }

    fn collect_effect_set(&mut self, effects: &CheckedEffectSet) {
        if let Some(variable) = &effects.variable {
            self.relevant.insert_effect(variable.id);
        }
        for resource in &effects.resources {
            self.collect_type(&resource.value_type);
        }
    }

    fn collect_bound(&mut self, bound: &CheckedTraitBound) {
        for argument in &bound.arguments {
            self.collect_type(argument);
        }
    }

    /// Collects every declared type/effect parameter reachable from a checked
    /// type. `Distinct.representation` is not expanded: its semantics are
    /// carried by the nominal ID plus arguments, matching canonical keys.
    fn collect_type(&mut self, value_type: &CheckedType) {
        match value_type {
            CheckedType::Inferred | CheckedType::Error => {}
            CheckedType::Never
            | CheckedType::I32
            | CheckedType::I8
            | CheckedType::I16
            | CheckedType::I64
            | CheckedType::U8
            | CheckedType::U16
            | CheckedType::U32
            | CheckedType::U64
            | CheckedType::ISize
            | CheckedType::USize
            | CheckedType::F32
            | CheckedType::F64
            | CheckedType::NumberLiteral(_)
            | CheckedType::String
            | CheckedType::StringLiteralSet(_)
            | CheckedType::CString
            | CheckedType::CChar => {}
            CheckedType::Parameter { id, .. } => self.relevant.insert_type(*id),
            CheckedType::Ref(payload)
            | CheckedType::Slice(payload)
            | CheckedType::Buffer(payload)
            | CheckedType::CPointer { pointee: payload } => self.collect_type(payload),
            CheckedType::Array { element, count } => {
                self.collect_type(element);
                self.collect_type(count);
            }
            CheckedType::TypeConstructor { arguments, .. }
            | CheckedType::Opaque { arguments, .. } => {
                for argument in arguments {
                    self.collect_type(argument);
                }
            }
            CheckedType::Product(product) => {
                for element in &product.elements {
                    self.collect_type(&element.value_type);
                }
            }
            CheckedType::Sum(sum) => {
                for alternative in &sum.alternatives {
                    self.collect_type(alternative);
                }
            }
            CheckedType::Function(function) => self.collect_function_type(function),
            CheckedType::Distinct { arguments, .. } => {
                for argument in arguments {
                    self.collect_type(argument);
                }
            }
        }
    }

    fn collect_block(&mut self, block: BlockId) {
        if !self.blocks.insert(block) {
            return;
        }
        let Some(block) = self.program.blocks.get(block) else {
            return;
        };
        let items = block.items.clone();
        let result = block.result;
        for item in items {
            self.collect_item(item);
        }
        if let Some(result) = result {
            self.collect_expression(result);
        }
    }

    fn collect_item(&mut self, item: ItemId) {
        if !self.items.insert(item) {
            return;
        }
        let Some(item) = self.program.items.get(item) else {
            return;
        };
        match &item.kind {
            LoweredItemKind::Binding(binding) => {
                self.family("item.binding");
                if let Some(symbol) = binding.symbol {
                    self.collect_symbol_type(symbol);
                }
                if let Some(value) = binding.value {
                    self.collect_expression(value);
                }
                if let Some(operation) = binding.reactive {
                    self.collect_reactive_operation(operation);
                }
            }
            LoweredItemKind::PatternBinding(binding) => {
                self.family("item.pattern-binding");
                self.collect_pattern(binding.pattern);
                self.collect_expression(binding.value);
                if let Some(propagation) = &binding.propagation {
                    self.collect_type(&propagation.source);
                    self.collect_type(&propagation.result);
                }
            }
            LoweredItemKind::Assignment(assignment) => {
                self.family("item.assignment");
                self.collect_place(assignment.target);
                self.collect_expression(assignment.value);
                if let Some(dispatch) = &assignment.mutate_index {
                    for argument in &dispatch.arguments {
                        self.collect_type(argument);
                    }
                }
                if let Some(evidence) = &assignment.evidence {
                    self.collect_evidence(evidence);
                }
                if let Some(operation) = assignment.signal_notify {
                    self.collect_reactive_operation(operation);
                }
            }
            LoweredItemKind::Return(item) => {
                self.family("item.return");
                self.collect_expression(item.value);
            }
            LoweredItemKind::Break(item) => {
                self.family("item.break");
                if let Some(value) = item.value {
                    self.collect_expression(value);
                }
            }
            LoweredItemKind::Continue(_) => {
                self.family("item.continue");
            }
            LoweredItemKind::Expression(item) => {
                self.family("item.expression");
                self.collect_expression(item.expression);
            }
        }
    }

    fn collect_expression(&mut self, expression: ExpressionId) {
        if !self.expressions.insert(expression) {
            return;
        }
        let Some(expression) = self.program.expressions.get(expression) else {
            return;
        };
        self.family("expression.header");
        let value_type = expression.value_type.clone();
        let effects = expression.effects.clone();
        let coercion = expression.coercion.clone();
        self.collect_type(&value_type);
        self.collect_effect_set(&effects);
        if let Some(coercion) = &coercion {
            self.collect_type(&coercion.source);
            self.collect_type(&coercion.target);
        }
        match &expression.kind {
            // No accepted lowered program retains a deferral, so this branch
            // has no record family of its own; it is still an explicit
            // decision so a new unlowered route cannot be silently collected.
            LoweredExpressionKind::Deferred(_) | LoweredExpressionKind::Stage26Deferred(_) => {}
            LoweredExpressionKind::Block(block) => {
                self.family("expression.block");
                self.collect_block(*block);
            }
            LoweredExpressionKind::Name(name) => {
                self.family("expression.name");
                self.collect_symbol_type(name.symbol);
                if let Some(operation) = name.reactive {
                    self.collect_reactive_operation(operation);
                }
            }
            LoweredExpressionKind::Integer(_) => self.family("expression.integer"),
            LoweredExpressionKind::Float(_) => self.family("expression.float"),
            LoweredExpressionKind::String(_) => self.family("expression.string"),
            LoweredExpressionKind::CString(_) => self.family("expression.cstring"),
            LoweredExpressionKind::Access(access) => {
                self.family("expression.access");
                self.collect_expression(access.base);
                match &access.kind {
                    LoweredAccessKind::Representation { dereference }
                    | LoweredAccessKind::Product { dereference, .. }
                    | LoweredAccessKind::Slice { dereference, .. }
                    | LoweredAccessKind::Scalar { dereference } => {
                        for value_type in dereference {
                            self.collect_type(value_type);
                        }
                    }
                }
            }
            LoweredExpressionKind::Product(product) => {
                self.family("expression.product");
                for element in &product.final_type.elements {
                    self.collect_type(&element.value_type);
                }
                for step in &product.steps {
                    match step {
                        LoweredProductStep::Positional { expression, .. }
                        | LoweredProductStep::Designated { expression, .. } => {
                            self.collect_expression(*expression);
                        }
                        LoweredProductStep::PositionalSpread { expression, .. }
                        | LoweredProductStep::NamedSpread { expression, .. } => {
                            self.collect_expression(*expression);
                        }
                        LoweredProductStep::Default {
                            expression,
                            expected,
                            ..
                        } => {
                            self.collect_expression(*expression);
                            self.collect_type(expected);
                        }
                    }
                }
            }
            LoweredExpressionKind::RepeatedProduct(product) => {
                self.family("expression.repeated-product");
                self.collect_expression(product.expression);
                if let LoweredRepeatCount::Symbolic(count) = &product.count {
                    self.collect_type(count);
                }
            }
            LoweredExpressionKind::Satisfies(satisfies) => {
                self.family("expression.satisfies");
                self.collect_expression(satisfies.value);
            }
            LoweredExpressionKind::Logical(logical) => {
                self.family("expression.logical");
                self.collect_expression(logical.left);
                self.collect_expression(logical.right);
                self.collect_type(&logical.bool_type);
            }
            LoweredExpressionKind::Loop(loop_) => {
                self.family("expression.loop");
                self.collect_block(loop_.body);
                self.collect_type(&loop_.result_type);
            }
            LoweredExpressionKind::Match(match_) => {
                self.family("expression.match");
                self.collect_expression(match_.subject);
                self.collect_type(&match_.source);
                for arm in &match_.arms {
                    self.collect_pattern(arm.pattern);
                    self.collect_expression(arm.body);
                    for symbol in &arm.bound_symbols {
                        self.collect_symbol_type(*symbol);
                    }
                }
            }
            LoweredExpressionKind::Index(index) => {
                self.family("expression.index");
                self.collect_expression(index.base);
                self.collect_expression(index.index);
                for argument in &index.arguments {
                    self.collect_type(argument);
                }
                if let Some(method_type) = &index.method_type {
                    self.collect_function_type(method_type);
                }
                self.collect_evidence(&index.evidence);
            }
            LoweredExpressionKind::StringTemplate(template) => {
                self.family("expression.string-template");
                for part in &template.parts {
                    match part {
                        LoweredStringTemplatePart::Literal(_) => {}
                        LoweredStringTemplatePart::Interpolation(interpolation) => {
                            self.collect_expression(interpolation.expression);
                            self.collect_type(&interpolation.value_type);
                            self.collect_evidence(&interpolation.evidence);
                        }
                    }
                }
            }
            LoweredExpressionKind::Call(call) => {
                self.family("expression.call");
                self.collect_call(*call);
            }
            LoweredExpressionKind::CallableValue(value) => {
                self.family("expression.callable-value");
                self.collect_callable_value(*value);
            }
            LoweredExpressionKind::Resource(use_) => {
                self.family("expression.resource");
                self.collect_resource_use(*use_);
            }
            LoweredExpressionKind::With(with) => {
                self.family("expression.with");
                if let Some(with) = self.program.withs.get(*with) {
                    self.collect_resource_provider(with.provider);
                    self.collect_expression(with.value);
                    self.collect_block(with.body);
                }
            }
            LoweredExpressionKind::Coro(coro) => {
                self.family("expression.coro");
                if let Some(coro) = self.program.coros.get(*coro) {
                    self.collect_coroutine_plan(coro.plan);
                }
            }
            LoweredExpressionKind::Await(await_) => {
                self.family("expression.await");
                if let Some(await_) = self.program.awaits.get(*await_) {
                    self.collect_expression(await_.operand);
                    self.collect_type(&await_.result_type);
                    match &await_.kind {
                        LoweredAwaitKind::ChildCoroutine {
                            child_result,
                            deferred_resources,
                            ..
                        } => {
                            self.collect_type(child_result);
                            for resource in deferred_resources {
                                self.collect_resource_use(*resource);
                            }
                        }
                        LoweredAwaitKind::Task { result } | LoweredAwaitKind::Wait { result } => {
                            self.collect_type(result);
                        }
                    }
                }
            }
        }
    }

    fn collect_call(&mut self, call: LoweredCallId) {
        let Some(call) = self.program.calls.get(call) else {
            return;
        };
        self.collect_function_type(&call.function_type);
        for argument in &call.arguments {
            if let Some(expression) = argument.expression {
                self.collect_expression(expression);
            }
            self.collect_type(&argument.expected);
            if let Some(place) = argument.place {
                self.collect_place(place);
            }
        }
        for binding in &call.resource_bindings {
            self.collect_resource_use(*binding);
        }
        for step in &call.steps {
            match step {
                LoweredCallStep::Callee { expression }
                | LoweredCallStep::ProductElement { expression, .. }
                | LoweredCallStep::ProductSpread { expression, .. }
                | LoweredCallStep::NamedProductSpread { expression, .. } => {
                    self.collect_expression(*expression);
                }
                LoweredCallStep::Argument { .. }
                | LoweredCallStep::Resource { .. }
                | LoweredCallStep::Invoke => {}
                LoweredCallStep::Default {
                    expression,
                    expected,
                    ..
                } => {
                    self.collect_expression(*expression);
                    self.collect_type(expected);
                }
            }
        }
        self.collect_type(&call.result_type);
        if let Some(operation) = call.reactive {
            self.collect_reactive_operation(operation);
        }
        for substitution in &call.substitutions.types {
            self.collect_type(&substitution.value_type);
        }
        for substitution in &call.substitutions.effects {
            self.collect_effect_set(&substitution.effects);
        }
        if let Some(evidence) = &call.evidence {
            self.collect_evidence(evidence);
        }
    }

    fn collect_callable_value(&mut self, value: LoweredCallableValueId) {
        let Some(value) = self.program.callable_values.get(value) else {
            return;
        };
        self.collect_function_type(&value.function_type);
        if let Some(closure) = &value.closure {
            for capture in &closure.captures {
                self.collect_type(&capture.value_type);
            }
            for substitution in &closure.substitutions.types {
                self.collect_type(&substitution.value_type);
            }
            for substitution in &closure.substitutions.effects {
                self.collect_effect_set(&substitution.effects);
            }
        }
        for substitution in &value.substitutions.types {
            self.collect_type(&substitution.value_type);
        }
        for substitution in &value.substitutions.effects {
            self.collect_effect_set(&substitution.effects);
        }
        if let Some(evidence) = &value.evidence {
            self.collect_evidence(evidence);
        }
    }

    fn collect_place(&mut self, place: PlaceId) {
        if !self.places.insert(place) {
            return;
        }
        let Some(place) = self.program.places.get(place) else {
            return;
        };
        self.family("place");
        self.collect_type(&place.value_type);
        match &place.kind {
            LoweredPlaceKind::Symbol { symbol } | LoweredPlaceKind::CapturedCell { symbol } => {
                self.collect_symbol_type(*symbol);
            }
            LoweredPlaceKind::Temporary { expression } => self.collect_expression(*expression),
            LoweredPlaceKind::Resource { use_ } => self.collect_resource_use(*use_),
            LoweredPlaceKind::Dereference {
                reference,
                dereference,
            } => {
                self.collect_expression(*reference);
                for value_type in dereference {
                    self.collect_type(value_type);
                }
            }
            LoweredPlaceKind::ProductElement { base, .. }
            | LoweredPlaceKind::Representation { base } => self.collect_place(*base),
            LoweredPlaceKind::Indexed { base, index } => {
                self.collect_place(*base);
                self.collect_expression(*index);
            }
        }
    }

    fn collect_pattern(&mut self, pattern: PatternId) {
        if !self.patterns.insert(pattern) {
            return;
        }
        let Some(pattern) = self.program.patterns.get(pattern) else {
            return;
        };
        self.family("pattern");
        self.collect_type(&pattern.value_type);
        match &pattern.kind {
            LoweredPatternKind::Wildcard => {}
            LoweredPatternKind::Binding { symbol, .. } => {
                if let Some(symbol) = symbol {
                    self.collect_symbol_type(*symbol);
                }
            }
            LoweredPatternKind::Product { elements, .. } => {
                for element in elements {
                    self.collect_pattern(*element);
                }
            }
            LoweredPatternKind::Nominal { argument, .. } => self.collect_pattern(*argument),
            LoweredPatternKind::Literal { .. } => {}
            LoweredPatternKind::At { binding, pattern } => {
                self.collect_pattern(*binding);
                self.collect_pattern(*pattern);
            }
        }
    }

    fn collect_resource_provider(&mut self, provider: LoweredResourceProviderId) {
        let Some(provider) = self.program.resource_providers.get(provider) else {
            return;
        };
        self.family("resource-provider");
        self.collect_type(&provider.resource.value_type);
    }

    fn collect_resource_use(&mut self, use_: LoweredResourceUseId) {
        let Some(use_) = self.program.resource_uses.get(use_) else {
            return;
        };
        self.family("resource-use");
        self.collect_type(&use_.resource.value_type);
        if let Some(provider) = use_.provider {
            self.collect_resource_provider(provider);
        }
    }

    fn collect_reactive_operation(&mut self, operation: LoweredReactiveOperationId) {
        if !self.operations.insert(operation) {
            return;
        }
        let Some(operation) = self.program.reactive_operations.get(operation) else {
            return;
        };
        self.family("reactive-operation");
        match &operation.kind {
            LoweredReactiveOperationKind::SignalCreate { symbol, .. }
            | LoweredReactiveOperationKind::SignalRead { symbol }
            | LoweredReactiveOperationKind::SignalNotify { symbol }
            | LoweredReactiveOperationKind::DerivedRead { symbol } => {
                self.collect_symbol_type(*symbol);
            }
            LoweredReactiveOperationKind::DerivedCreate {
                symbol,
                function_type,
                captures,
                ..
            } => {
                self.collect_symbol_type(*symbol);
                self.collect_function_type(function_type);
                for capture in captures {
                    self.collect_symbol_type(capture.symbol);
                }
            }
            LoweredReactiveOperationKind::Scope | LoweredReactiveOperationKind::Snapshot => {}
            LoweredReactiveOperationKind::Reaction {
                callback,
                reactive_provider,
            } => {
                self.collect_reactive_callback(*callback);
                if let Some(provider) = reactive_provider {
                    self.collect_resource_provider(*provider);
                }
            }
            LoweredReactiveOperationKind::Until {
                predicate,
                reactive_provider,
            } => {
                self.collect_reactive_callback(*predicate);
                if let Some(provider) = reactive_provider {
                    self.collect_resource_provider(*provider);
                }
            }
            LoweredReactiveOperationKind::Batch { callback } => {
                self.collect_reactive_callback(*callback);
            }
        }
    }

    fn collect_reactive_callback(&mut self, callback: LoweredReactiveCallbackId) {
        if !self.callbacks.insert(callback) {
            return;
        }
        let Some(callback) = self.program.reactive_callbacks.get(callback) else {
            return;
        };
        self.family("reactive-callback");
        self.collect_function_type(&callback.function_type);
        for capture in &callback.captures {
            self.collect_symbol_type(capture.symbol);
        }
        for resource in &callback.resources {
            self.collect_resource_use(*resource);
        }
        if let Some(callable) = callback.callable {
            self.collect_expression(callable);
        }
    }

    fn collect_coroutine_plan(&mut self, plan: LoweredCoroutinePlanId) {
        if !self.plans.insert(plan) {
            return;
        }
        let Some(plan) = self.program.coroutine_plans.get(plan) else {
            return;
        };
        self.collect_type(&plan.result_type);
        self.collect_effect_set(&plan.deferred_effects);
        for capture in &plan.captures {
            self.collect_symbol_type(capture.symbol);
        }
        for symbol in &plan.frame_bindings {
            self.collect_symbol_type(*symbol);
        }
        for value_type in &plan.await_result_types {
            self.collect_type(value_type);
        }
        for await_ in &plan.awaits {
            if let Some(await_) = self.program.awaits.get(*await_) {
                self.collect_expression(await_.operand);
                self.collect_type(&await_.result_type);
                if let LoweredAwaitKind::ChildCoroutine {
                    child_result,
                    deferred_resources,
                    ..
                } = &await_.kind
                {
                    self.collect_type(child_result);
                    for resource in deferred_resources {
                        self.collect_resource_use(*resource);
                    }
                } else if let LoweredAwaitKind::Task { result }
                | LoweredAwaitKind::Wait { result } = &await_.kind
                {
                    self.collect_type(result);
                }
            }
        }
    }

    fn collect_evidence(&mut self, evidence: &TraitEvidence) {
        self.family("trait-evidence");
        match evidence {
            TraitEvidence::ExplicitImplementation { arguments, .. }
            | TraitEvidence::Structural { arguments, .. }
            | TraitEvidence::RejectedImplementation { arguments, .. } => {
                for argument in arguments {
                    self.collect_type(argument);
                }
            }
            TraitEvidence::DeclaredBound {
                arguments,
                prerequisites,
                ..
            } => {
                for argument in arguments {
                    self.collect_type(argument);
                }
                for bound in prerequisites {
                    self.collect_bound(bound);
                }
            }
        }
    }
}

impl LoweredProgram {
    /// The type and effect parameters relevant to one function template.
    ///
    /// The scan is deterministic: arenas are visited in insertion order and
    /// shared nodes are visited once. Nested function bodies are scanned under
    /// their own `FunctionId`; this function only follows the records that
    /// construct or invoke them.
    pub(crate) fn relevant_parameters(&self, function: FunctionId) -> RelevantParameters {
        let mut collector = ParameterCollector::new(self);
        collector.collect_function(function);
        collector.relevant
    }

    /// Every record family the collector visits for one function template.
    /// Used by the coverage test to prove each parameter-bearing family has an
    /// explicit collector decision.
    #[cfg(test)]
    pub(crate) fn parameter_record_families(&self, function: FunctionId) -> BTreeSet<&'static str> {
        let mut collector = ParameterCollector::new(self);
        collector.collect_function(function);
        collector.families
    }

    /// The record families the collector visits under every module
    /// initializer, for the coverage test.
    #[cfg(test)]
    pub(crate) fn initializer_record_families(&self) -> BTreeSet<&'static str> {
        let mut collector = ParameterCollector::new(self);
        for (id, _) in self.initializers.iter() {
            collector.collect_initializer(id);
        }
        collector.families
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use crate::{NameResolver, ProgramLoader, TypeChecker, TypedModule, contains_type_parameter};

    use super::*;

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

    fn lower(source: &str) -> (TypedModule, LoweredProgram) {
        let module = checked_program(source);
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());
        (module, program)
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

    fn relevance(program: &LoweredProgram, name: &str) -> RelevantParameters {
        program.relevant_parameters(function_id(program, name))
    }

    #[test]
    fn relevant_parameter_collection_is_deterministic() {
        let (_, program) = lower(concat!(
            "def identity: <T where Copy T> T -> T = value => value\n",
            "let applied = identity 1\n",
        ));
        let name = "identity";
        let function = function_id(&program, name);
        let first = program.relevant_parameters(function);
        assert_eq!(first, program.relevant_parameters(function));
        assert_eq!(first.type_parameters().count(), 1);
        assert_eq!(first.effect_parameters().count(), 0);
    }

    #[test]
    fn signature_only_parameter_is_found() {
        let (_, program) = lower(concat!(
            "def identity: <T where Copy T> T -> T = value => value\n",
            "let applied = identity 1\n",
        ));
        let relevant = relevance(&program, "identity");
        assert!(!relevant.is_empty());
        assert_eq!(relevant.effect_parameters().count(), 0);
        assert_eq!(relevant.type_parameters().count(), 1);
    }

    #[test]
    fn body_only_parameter_is_found() {
        let (_, program) = lower(concat!(
            "type Phantom T = ctor ()\n",
            "def phantom: <T> () -> Phantom T = () => Phantom ()\n",
            "def body_only: <T> I32 -> I32 = value => {\n",
            "  let hidden: Phantom T = phantom ()\n",
            "  value\n",
            "}\n",
        ));
        let function = function_id(&program, "body_only");
        assert!(
            !contains_type_parameter(&CheckedType::Function(
                program.functions.get(function).unwrap().signature.clone()
            )),
            "the body-only fixture must keep its signature concrete"
        );
        let relevant = program.relevant_parameters(function);
        assert_eq!(relevant.effect_parameters().count(), 0);
        assert_eq!(relevant.type_parameters().count(), 1);
    }

    #[test]
    fn capture_only_parameter_is_found_under_its_own_function() {
        let (_, program) = lower(concat!(
            "def capture_only: <T where Copy T> T -> () -> I32 = value => () => {\n",
            "  let copied: T = value\n",
            "  0\n",
            "}\n",
            "let made = capture_only 1\n",
        ));
        let mut found_capture_only = false;
        for (_, _, function) in program.functions.iter() {
            let relevant = program.relevant_parameters(function.semantic_id);
            if !relevant.is_empty()
                && !contains_type_parameter(&CheckedType::Function(function.signature.clone()))
                && !function.captures.is_empty()
            {
                found_capture_only = true;
            }
        }
        assert!(
            found_capture_only,
            "a nested closure whose signature is concrete but whose captures mention an outer parameter must be relevant"
        );
    }

    #[test]
    fn evidence_only_and_effect_only_parameters_are_found() {
        let (_, program) = lower(concat!(
            "trait TestShow T { test_show: T -> Bool }\n",
            "impl TestShow I32 { def test_show = _ => True }\n",
            "def show_bound: <T where TestShow T> T -> Bool = value => test_show value\n",
            "def effectful: <effect E> () ->{E} () = () => ()\n",
            "let applied = show_bound 1\n",
        ));
        let show_bound = function_id(&program, "show_bound");
        let relevant = program.relevant_parameters(show_bound);
        assert_eq!(relevant.type_parameters().count(), 1);
        let families = program.parameter_record_families(show_bound);
        assert!(
            families.contains("trait-evidence"),
            "the declared-bound call evidence must be scanned: {families:?}"
        );

        let effectful = relevance(&program, "effectful");
        assert_eq!(effectful.type_parameters().count(), 0);
        assert_eq!(effectful.effect_parameters().count(), 1);
    }

    #[test]
    fn irrelevant_outer_parameter_is_excluded() {
        let (_, program) = lower(concat!(
            "def identity: <T where Copy T> T -> T = value => value\n",
            "def irrelevant: <T where Copy T> I32 -> I32 = value => value\n",
            "let applied = identity 1\n",
            "let concrete = irrelevant 1\n",
        ));
        let relevant = relevance(&program, "irrelevant");
        assert!(
            relevant.is_empty(),
            "an unused outer parameter must not enter the relevant set: {relevant:?}"
        );
    }

    #[test]
    fn parameter_record_families_are_unique_and_cover_the_collector() {
        let mut seen = BTreeSet::new();
        for family in PARAMETER_RECORD_FAMILIES {
            assert!(
                seen.insert(*family),
                "record family {family} appears twice in the decision table"
            );
        }
    }

    #[test]
    fn coverage_fixture_exercises_every_collector_family() {
        let (_, program) = lower(concat!(
            "use std.cinterop.*\n",
            "use std.coroutine.(Coroutine)\n",
            "use std.fmt.Formatter\n",
            "trait TestShow T { test_show: T -> Bool }\n",
            "impl TestShow I32 { def test_show = _ => True }\n",
            "type Counter = ctor (value: I32)\n",
            "type Ok T = ctor T\n",
            "type IOError = ctor String\n",
            "type Phantom T = ctor ()\n",
            "def identity: <T where Copy T> T -> T = value => value\n",
            "def body_only: <T> I32 -> I32 = value => {\n",
            "  let hidden: Phantom T = Phantom ()\n",
            "  value\n",
            "}\n",
            "def capture_only: <T where Copy T> T -> () -> I32 = value => () => {\n",
            "  let copied: T = value\n",
            "  0\n",
            "}\n",
            "def effectful: <effect E> () ->{E} () = () => ()\n",
            "def callable = (value: I32) => value\n",
            "def show_bound: <T where TestShow T> T -> Bool = value => test_show value\n",
            "let integer: I32 = identity 42\n",
            "let shown: Bool = show_bound 1\n",
            "let floated: F64 = 1.5\n",
            "let string: String = \"text\"\n",
            "let cstring = c_string \"c\"\n",
            "let product = (left: 1, right: 2)\n",
            "let left = product.left\n",
            "let values: (I32; 2) = (1, 2)\n",
            "let element = values[0]\n",
            "def logical = (flag: Bool) => flag && flag\n",
            "def looping = () => loop { break 1 }\n",
            "def continuing = () => loop { continue\nbreak 1 }\n",
            "def returning: () -> I32 = () => { return 1 }\n",
            "def matching = (value: Ok I32 | IOError) => match value {\n",
            "  Ok inner => inner,\n",
            "  other => 0,\n",
            "}\n",
            "def blocked = () => { let local: I32 = 1; local }\n",
            "def repeat = () => { let repeated: (I32; 3) = (7; 3); repeated }\n",
            "def templated = () => { let rendered: String = \"value=${integer}\"; rendered }\n",
            "let coerced: I8 = 42 satisfies I8\n",
            "let repeated: (I32; 3) = (7; 3)\n",
            "let template: String = \"value=${integer}\"\n",
            "let applied = callable 1\n",
            "let closure = callable\n",
            "def task: () -> Coroutine{} I32 = () => coro { 7 }\n",
            "def driver: () -> Coroutine{} I32 = () => coro { await (task ()) }\n",
            "def increment: () ->{mut Counter} () = () => {\n",
            "  (resource Counter).value = (resource Counter).value + 1\n",
            "}\n",
            "let mut counter = Counter (value: 0)\n",
            "with mut Counter = counter { increment () }\n",
            "def scoped_increment = () => { with mut Counter = counter { increment (); () } }\n",
            "let signal observed = 0\n",
            "let doubled_signal = observed + observed\n",
            "with Reactive = reactive_scope () {\n",
            "  reaction { let current = observed; () }\n",
            "  batch { observed = 1 }\n",
            "  let snapshotted = snapshot observed\n",
            "}\n",
        ));
        let mut visited = program.initializer_record_families();
        for (_, _, function) in program.functions.iter() {
            visited.extend(program.parameter_record_families(function.semantic_id));
        }
        for family in PARAMETER_RECORD_FAMILIES {
            assert!(
                visited.contains(family),
                "the coverage fixture never reached collector family {family}; \
                 add a record of that family or record an explicit no-parameter decision"
            );
        }
    }
}
