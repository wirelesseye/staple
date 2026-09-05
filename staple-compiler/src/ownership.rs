use std::collections::{HashMap, HashSet};

use crate::{FunctionId, ResolvedFunction, SymbolId, TypedModule};
use staple_syntax::{Diagnostic, Expression, Item, Pattern, Syntax, SyntaxId};

#[derive(Debug, Clone, Default)]
pub(crate) struct OwnershipInfo {
    moved_uses: HashMap<SyntaxId, HashSet<SymbolId>>,
    non_owning_symbols: HashSet<SymbolId>,
    borrowed_captures: HashSet<(FunctionId, SymbolId)>,
}

impl OwnershipInfo {
    pub(crate) fn moved_symbols(&self, syntax: SyntaxId) -> impl Iterator<Item = SymbolId> + '_ {
        self.moved_uses
            .get(&syntax)
            .into_iter()
            .flat_map(|symbols| symbols.iter().copied())
    }

    pub(crate) fn is_non_owning_symbol(&self, symbol: SymbolId) -> bool {
        self.non_owning_symbols.contains(&symbol)
    }

    pub(crate) fn is_borrowed_capture(&self, function: FunctionId, symbol: SymbolId) -> bool {
        self.borrowed_captures.contains(&(function, symbol))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BorrowKind {
    Shared,
    Mutable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BorrowOrigin {
    source: SymbolId,
    kind: BorrowKind,
}

#[derive(Debug, Clone, Default)]
struct BorrowResultSummary {
    parameters: Vec<(usize, BorrowKind)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValueState {
    Available,
    Moved,
    MaybeMoved,
    Frozen,
}

/// Checks Staple's affine ownership rules after ordinary type checking has
/// resolved every expression and pattern type.
pub(crate) struct OwnershipChecker<'a> {
    module: &'a TypedModule,
    function: Option<FunctionId>,
    states: HashMap<SymbolId, ValueState>,
    top_level_symbols: HashSet<SymbolId>,
    info: OwnershipInfo,
    diagnostics: Vec<Diagnostic>,
    loops: Vec<LoopOwnershipContext>,
    borrow_summaries: HashMap<FunctionId, BorrowResultSummary>,
    borrowed_closures: HashMap<SymbolId, Vec<BorrowOrigin>>,
    active_borrows: HashMap<SymbolId, (BorrowKind, usize)>,
    borrow_scopes: Vec<Vec<SymbolId>>,
    parameter_symbols: HashSet<SymbolId>,
}

#[derive(Default)]
struct LoopOwnershipContext {
    breaks: Vec<HashMap<SymbolId, ValueState>>,
    back_edges: Vec<HashMap<SymbolId, ValueState>>,
}

impl<'a> OwnershipChecker<'a> {
    pub(crate) fn check(module: &'a TypedModule) -> (OwnershipInfo, Vec<Diagnostic>) {
        let mut checker = Self {
            module,
            function: None,
            states: HashMap::new(),
            top_level_symbols: HashSet::new(),
            info: OwnershipInfo::default(),
            diagnostics: vec![],
            loops: vec![],
            borrow_summaries: infer_borrow_summaries(module),
            borrowed_closures: HashMap::new(),
            active_borrows: HashMap::new(),
            borrow_scopes: vec![],
            parameter_symbols: HashSet::new(),
        };
        checker.collect_top_level_symbols();
        checker.check_top_level();

        for function in module.functions() {
            if function
                .binding_syntax
                .and_then(|syntax| module.symbol_for(syntax))
                .is_some_and(|symbol| module.resolved().intrinsic_function(symbol).is_some())
            {
                continue;
            }
            checker.check_function(function);
        }
        for function in module.implicit_thunks() {
            checker.check_function(function);
        }

        (checker.info, checker.diagnostics)
    }

    /// Collects the symbol of every top-level `let`/pattern binding across
    /// every module, so `use_symbol` can recognize a global in O(1) instead
    /// of rescanning every module's items on each use.
    fn collect_top_level_symbols(&mut self) {
        for source_module in self.module.resolved().program().modules() {
            for item in &source_module.syntax.items {
                match item {
                    Item::Binding(binding) => {
                        if let Some(symbol) = self.module.symbol_for(binding.syntax.id) {
                            self.top_level_symbols.insert(symbol);
                        }
                    }
                    Item::PatternBinding(binding) => {
                        self.collect_top_level_pattern(&binding.pattern);
                    }
                    _ => {}
                }
            }
        }
    }

    fn collect_top_level_pattern(&mut self, pattern: &Pattern) {
        match pattern {
            Pattern::Binding(binding) => {
                if let Some(symbol) = self.module.symbol_for(binding.syntax.id) {
                    self.top_level_symbols.insert(symbol);
                }
            }
            Pattern::At(at) => {
                self.collect_top_level_pattern(&Pattern::Binding(at.binding.as_ref().clone()));
                self.collect_top_level_pattern(&at.pattern);
            }
            Pattern::Product(product) => {
                for element in &product.elements {
                    self.collect_top_level_pattern(element);
                }
            }
            Pattern::Nominal(nominal) => self.collect_top_level_pattern(&nominal.argument),
            Pattern::Wildcard(_) | Pattern::StringLiteral(_) | Pattern::Splice(_) => {}
        }
    }

    /// Runs the same per-item ownership checks used for a function body over
    /// each module's top-level statement sequence. Every symbol bound here is
    /// a top-level symbol (see `collect_top_level_symbols`), so `use_symbol`
    /// intercepts moves of it as a global before ever consulting `states` —
    /// the `states` entries `check_item` inserts along the way are harmless,
    /// unread bookkeeping for those symbols.
    fn check_top_level(&mut self) {
        for source_module in self.module.resolved().program().modules() {
            self.function = None;
            self.states.clear();
            self.loops.clear();
            for item in &source_module.syntax.items {
                if !self.check_item(item) {
                    break;
                }
            }
        }
    }

    fn check_function(&mut self, function: &ResolvedFunction) {
        self.function = Some(function.id);
        self.states.clear();
        self.loops.clear();
        self.borrowed_closures.clear();
        self.active_borrows.clear();
        self.borrow_scopes.clear();
        self.parameter_symbols = top_level_parameter_symbols(self.module, &function.pattern)
            .into_iter()
            .flatten()
            .collect();

        let drop_method = self.module.is_drop_method(function.id);
        self.bind_function_pattern(&function.pattern, drop_method);
        for capture in &function.captures {
            let Some(value_type) = self.module.type_of_symbol(*capture) else {
                continue;
            };
            let state = if self
                .module
                .is_copy_in_function(value_type, Some(function.id))
                || self.module.has_mutable_storage(*capture)
            {
                ValueState::Available
            } else {
                ValueState::Frozen
            };
            self.states.insert(*capture, state);
        }

        if self.borrow_origins(&function.body).is_some() {
            self.diagnostics.push(Diagnostic::new(
                function.body.syntax().span.clone(),
                "a borrowed closure cannot escape its lexical scope",
            ));
        }
        self.check_expression(&function.body, true);
    }

    /// Returns whether control continues after the expression.
    fn check_expression(&mut self, expression: &Expression, consume: bool) -> bool {
        match expression {
            Expression::Function(value) => {
                if let Some(function_id) = self.module.function_for(value.syntax.id) {
                    let captures = self.module.functions()[function_id.0].captures.clone();
                    for capture in captures {
                        if self.borrowed_closures.contains_key(&capture) {
                            self.diagnostics.push(Diagnostic::new(
                                value.syntax.span.clone(),
                                "a borrowed closure cannot be captured by another closure",
                            ));
                        } else if self.parameter_symbols.contains(&capture)
                            && !self.module.is_move_parameter(capture)
                            && self
                                .module
                                .type_of_symbol(capture)
                                .is_some_and(|ty| {
                                    !self.module.is_copy_in_function(ty, self.function)
                                })
                        {
                            self.info.borrowed_captures.insert((function_id, capture));
                        } else if !self.module.has_mutable_storage(capture) {
                            self.use_symbol(capture, &value.syntax, true);
                        }
                    }
                }
                true
            }
            Expression::Satisfies(value) => self.check_expression(&value.value, consume),
            Expression::Match(value) => {
                self.check_expression(&value.subject, true);
                let outer = self.states.clone();
                let mut continuing = vec![];
                for arm in &value.arms {
                    self.states = outer.clone();
                    self.bind_pattern(&arm.pattern, false);
                    if self.check_expression(&arm.body, consume) {
                        continuing.push(self.states.clone());
                    }
                }
                self.states = merge_states(&outer, &continuing);
                !continuing.is_empty()
            }
            Expression::Loop(value) => {
                let entry = self.states.clone();
                self.loops.push(LoopOwnershipContext::default());
                if self.check_expression(&Expression::Block(value.body.clone()), false) {
                    self.loops
                        .last_mut()
                        .expect("loop ownership context")
                        .back_edges
                        .push(self.states.clone());
                }
                let context = self.loops.pop().expect("loop ownership context");
                for (symbol, initial) in &entry {
                    if *initial == ValueState::Available
                        && context.back_edges.iter().any(|state| {
                            matches!(
                                state.get(symbol),
                                Some(ValueState::Moved | ValueState::MaybeMoved)
                            )
                        })
                    {
                        self.diagnostics.push(Diagnostic::new(
                            value.syntax.span.clone(),
                            "move-only value may be moved before the next loop iteration",
                        ));
                    }
                }
                self.states = merge_states(&entry, &context.breaks);
                !context.breaks.is_empty()
            }
            Expression::Resource(value) => {
                if consume
                    && self
                        .module
                        .resource_for_expression(value.syntax.id)
                        .is_some_and(|resource| {
                            !self
                                .module
                                .is_copy_in_function(&resource.value_type, self.function)
                        })
                {
                    self.diagnostics.push(Diagnostic::new(
                        value.syntax.span.clone(),
                        "cannot move out of a borrowed resource",
                    ));
                }
                true
            }
            Expression::With(value) => {
                self.check_expression(&value.value, false);
                self.check_expression(&Expression::Block(value.body.clone()), consume)
            }
            Expression::Block(value) => {
                self.borrow_scopes.push(vec![]);
                for item in &value.items {
                    if !self.check_item(item) {
                        self.end_borrow_scope();
                        return false;
                    }
                }
                self.end_borrow_scope();
                true
            }
            Expression::Product(value) => {
                for element in &value.elements {
                    self.reject_borrowed_escape(&element.value);
                    self.check_expression(&element.value, consume);
                }
                self.check_product_defaults(value.syntax.id, consume);
                true
            }
            Expression::RepeatedProduct(value) => {
                self.check_expression(&value.value, consume);
                self.check_expression(&value.count, false);
                true
            }
            Expression::Call(value) => {
                self.reject_borrowed_argument(&value.argument);
                if let Some(plan) = self.module.juxtaposed_call_plan(value.syntax.id) {
                    let expected = match plan.function.parameter.as_ref() {
                        crate::CheckedType::Product(product) => product.elements.len(),
                        _ => 0,
                    };
                    if plan.arguments.len() == expected {
                        let mut callee = value.callee.as_ref();
                        for _ in 1..plan.consumed_calls {
                            let Expression::Call(previous) = callee else {
                                break;
                            };
                            callee = previous.callee.as_ref();
                        }
                        self.check_expression(callee, false);
                        for (index, argument) in plan.arguments.iter().enumerate() {
                            if let Some(thunk) =
                                self.module.implicit_thunk_for(argument.syntax().id)
                            {
                                for capture in &thunk.captures {
                                    if !self.module.has_mutable_storage(*capture) {
                                        self.use_symbol(*capture, argument.syntax(), true);
                                    }
                                }
                            } else {
                                self.check_expression(
                                    argument,
                                    plan.function
                                        .moves
                                        .contains(&crate::CheckedMutation::Element(index)),
                                );
                            }
                        }
                        return self.expression_continues(expression);
                    }
                }
                self.check_expression(&value.callee, false);
                if let Some(plan) = self.module.curried_default_plan(value.syntax.id) {
                    for default in &plan.defaults {
                        self.check_call_argument(&default.value, Some(&default.function));
                    }
                    if !matches!(value.argument.as_ref(), Expression::Name(name) if name.name == "_")
                    {
                        let residual = plan.defaults.last().and_then(|default| {
                            match default.function.result.as_ref() {
                                crate::CheckedType::Function(function) => Some(function),
                                _ => None,
                            }
                        });
                        self.check_call_argument(&value.argument, residual);
                    }
                    return self.expression_continues(expression);
                }
                let scoped_c_string = self
                    .module
                    .symbol_for(value.callee.syntax().id)
                    .is_some_and(|symbol| self.module.resolved().is_external_symbol(symbol))
                    && self
                        .module
                        .type_of_expression(value.argument.syntax().id)
                        .is_some_and(|ty| matches!(ty, crate::CheckedType::CString));
                if let Some(thunk) = self.module.implicit_thunk_for(value.argument.syntax().id) {
                    for capture in &thunk.captures {
                        if !self.module.has_mutable_storage(*capture) {
                            self.use_symbol(*capture, value.argument.syntax(), true);
                        }
                    }
                } else if scoped_c_string {
                    self.check_expression(&value.argument, false);
                } else {
                    let callee_function_type = self
                        .module
                        .type_of_expression(value.callee.syntax().id)
                        .or_else(|| {
                            self.module
                                .symbol_for(value.callee.syntax().id)
                                .and_then(|symbol| self.module.type_of_symbol(symbol))
                        })
                        .and_then(|ty| match ty {
                            crate::CheckedType::Function(function) => Some(function),
                            _ => None,
                        });
                    self.check_call_argument(&value.argument, callee_function_type);
                }
                self.expression_continues(expression)
            }
            Expression::Access(value) => {
                if let Some(symbol) = self.module.symbol_for(value.syntax.id) {
                    self.use_symbol(symbol, &value.syntax, consume);
                } else {
                    let result_is_copy = self
                        .module
                        .type_of_expression(value.syntax.id)
                        .is_none_or(|ty| self.module.is_copy_in_function(ty, self.function));
                    if consume && !result_is_copy {
                        self.diagnostics.push(Diagnostic::new(
                            value.syntax.span.clone(),
                            "cannot move a field out through an accessor; destructure the whole value",
                        ));
                    }
                    self.check_expression(&value.value, false);
                }
                true
            }
            Expression::Index(value) => {
                self.check_expression(&value.value, false);
                self.check_expression(&value.index, true);
                true
            }
            Expression::Logical(value) => {
                self.check_expression(&value.left, true);
                let outer = self.states.clone();
                let short_circuit_states = outer.clone();
                let right_continues = self.check_expression(&value.right, consume);
                let mut continuing = vec![short_circuit_states];
                if right_continues {
                    continuing.push(self.states.clone());
                }
                self.states = merge_states(&outer, &continuing);
                true
            }
            Expression::Name(value) => {
                if let Some(symbol) = self.module.symbol_for(value.syntax.id) {
                    self.use_symbol(symbol, &value.syntax, consume);
                }
                true
            }
            Expression::StringTemplate(template) => {
                for part in &template.parts {
                    if let staple_syntax::StringTemplatePart::Interpolation(interpolation) = part {
                        self.check_expression(&interpolation.expression, true);
                    }
                }
                true
            }
            Expression::SyntaxArgument(_)
            | Expression::VisibilityArgument(_)
            | Expression::Quote(_)
            | Expression::Splice(_) => true,
            Expression::String(_)
            | Expression::CString(_)
            | Expression::Integer(_)
            | Expression::Float(_) => true,
            Expression::Binary(_) => unreachable!("binary expression reached ownership checking"),
        }
    }

    fn expression_continues(&self, expression: &Expression) -> bool {
        !self
            .module
            .coercion_for(expression.syntax().id)
            .is_some_and(|coercion| coercion.source == crate::CheckedType::Never)
            && !self
                .module
                .type_of_expression(expression.syntax().id)
                .is_some_and(|value_type| *value_type == crate::CheckedType::Never)
    }

    /// Checks a call's argument against the callee's per-position parameter
    /// modes: a position is consuming only when the callee marks it `move`
    /// (or the whole parameter is `move`d as one unit). `mut` positions and
    /// ordinary non-`Copy` positions (the new implicit-borrow default) are
    /// non-consuming — `use_symbol` already no-ops for `Copy` positions
    /// regardless of `consume`, so this doesn't need to consult Copy-ness
    /// itself. When the argument is a literal product matching the callee's
    /// product-shaped parameter, each element is checked against its own
    /// position; otherwise the whole argument is consumed only if the
    /// callee requires a move somewhere in it, matching the existing rule
    /// that a partial move out of a single bound value is rejected.
    fn check_call_argument(
        &mut self,
        argument: &Expression,
        callee: Option<&crate::CheckedFunctionType>,
    ) {
        let Some(callee) = callee else {
            self.check_expression(argument, true);
            return;
        };
        if callee.mutations.contains(&crate::CheckedMutation::Whole) {
            if let Some(symbol) = self.module.symbol_for(argument.syntax().id) {
                self.check_borrow_conflict(symbol, BorrowKind::Mutable, argument.syntax());
            }
        }
        if callee.moves.contains(&crate::CheckedMutation::Whole) {
            self.check_expression(argument, true);
            return;
        }
        if let Expression::Product(product) = argument
            && let crate::CheckedType::Product(parameter) = callee.parameter.as_ref()
            && product.elements.len() == parameter.elements.len()
        {
            for (index, element) in product.elements.iter().enumerate() {
                if callee
                    .mutations
                    .contains(&crate::CheckedMutation::Element(index))
                    && let Some(symbol) = self.module.symbol_for(element.value.syntax().id)
                {
                    self.check_borrow_conflict(
                        symbol,
                        BorrowKind::Mutable,
                        element.value.syntax(),
                    );
                }
                let consume = callee
                    .moves
                    .contains(&crate::CheckedMutation::Element(index));
                self.check_expression(&element.value, consume);
            }
            return;
        }
        self.check_expression(argument, !callee.moves.is_empty());
        if !matches!(argument, Expression::Product(_))
            && let Some(plan) = self
                .module
                .product_default_plan(argument.syntax().id)
                .cloned()
        {
            for (index, default) in plan.defaults.into_iter().enumerate() {
                if let Some(default) = default {
                    self.check_expression(
                        &default,
                        callee
                            .moves
                            .contains(&crate::CheckedMutation::Element(index)),
                    );
                }
            }
        }
    }

    fn check_product_defaults(&mut self, syntax: staple_syntax::SyntaxId, consume: bool) {
        let Some(plan) = self.module.product_default_plan(syntax).cloned() else {
            return;
        };
        for default in plan.defaults.into_iter().flatten() {
            self.check_expression(&default, consume);
        }
    }

    fn borrow_origins(&self, expression: &Expression) -> Option<Vec<BorrowOrigin>> {
        match expression {
            Expression::Satisfies(value) => return self.borrow_origins(&value.value),
            Expression::Block(block) => {
                return match block.items.last()? {
                    Item::Expression(value) => self.borrow_origins(value),
                    Item::Return(value) => self.borrow_origins(&value.value),
                    _ => None,
                };
            }
            Expression::Match(value) => {
                let mut origins = value
                    .arms
                    .iter()
                    .map(|arm| self.borrow_origins(&arm.body));
                let first = origins.next()??;
                return origins
                    .all(|origin| origin.as_ref() == Some(&first))
                    .then_some(first);
            }
            _ => {}
        }
        if let Some(symbol) = self.module.symbol_for(expression.syntax().id)
            && let Some(origin) = self.borrowed_closures.get(&symbol)
        {
            return Some(origin.clone());
        }
        let Expression::Call(call) = expression else {
            return None;
        };
        let function = self.static_callee(&call.callee)?;
        let summary = self.borrow_summaries.get(&function)?;
        summary
            .parameters
            .iter()
            .map(|(position, kind)| {
                let argument = call_argument_at(&call.argument, *position)?;
                let source = self.module.symbol_for(argument.syntax().id)?;
                Some(BorrowOrigin {
                    source,
                    kind: *kind,
                })
            })
            .collect()
    }

    fn direct_closure_borrow_origins(&self, expression: &Expression) -> Option<Vec<BorrowOrigin>> {
        let Expression::Function(function) = expression else {
            return None;
        };
        let function = self.module.function_for(function.syntax.id)?;
        let closure = self.module.function_by_id(function)?;
        let origins = closure
            .captures
            .iter()
            .filter(|capture| {
                self.parameter_symbols.contains(capture)
                    && !self.module.is_move_parameter(**capture)
                    && self.module.type_of_symbol(**capture).is_some_and(|ty| {
                        !self.module.is_copy_in_function(ty, self.function)
                    })
            })
            .map(|capture| BorrowOrigin {
                source: *capture,
                kind: if self.module.is_mutated_parameter(*capture) {
                    BorrowKind::Mutable
                } else {
                    BorrowKind::Shared
                },
            })
            .collect::<Vec<_>>();
        (!origins.is_empty()).then_some(origins)
    }

    fn static_callee(&self, expression: &Expression) -> Option<FunctionId> {
        self.module
            .function_for(expression.syntax().id)
            .or_else(|| {
                self.module
                    .symbol_for(expression.syntax().id)
                    .and_then(|symbol| self.module.function_for_symbol(symbol))
            })
    }

    fn reject_borrowed_escape(&mut self, expression: &Expression) {
        if self.borrow_origins(expression).is_some() || self.is_borrow_producer_reference(expression)
        {
            self.diagnostics.push(Diagnostic::new(
                expression.syntax().span.clone(),
                "a borrowed closure cannot escape its lexical scope",
            ));
        }
    }

    fn reject_borrowed_argument(&mut self, expression: &Expression) {
        if self.borrow_origins(expression).is_some() || self.is_borrow_producer_reference(expression)
        {
            self.diagnostics.push(Diagnostic::new(
                expression.syntax().span.clone(),
                "a borrowed closure cannot be passed as an argument",
            ));
        }
    }

    fn is_borrow_producer_reference(&self, expression: &Expression) -> bool {
        matches!(expression, Expression::Name(_) | Expression::Access(_))
            && self
                .static_callee(expression)
                .is_some_and(|function| self.borrow_summaries.contains_key(&function))
    }

    fn check_borrow_conflict(&mut self, symbol: SymbolId, requested: BorrowKind, syntax: &Syntax) {
        let Some((active, _)) = self.active_borrows.get(&symbol).copied() else {
            return;
        };
        if requested == BorrowKind::Mutable || active == BorrowKind::Mutable {
            self.diagnostics.push(Diagnostic::new(
                syntax.span.clone(),
                "cannot mutate a value while it is borrowed by a closure",
            ));
        }
    }

    fn end_borrow_scope(&mut self) {
        let Some(symbols) = self.borrow_scopes.pop() else {
            return;
        };
        for symbol in symbols {
            if let Some(origins) = self.borrowed_closures.remove(&symbol) {
                for origin in origins {
                    if let Some((_, count)) = self.active_borrows.get_mut(&origin.source) {
                        *count -= 1;
                        if *count == 0 {
                            self.active_borrows.remove(&origin.source);
                        }
                    }
                }
            }
        }
    }

    fn check_item(&mut self, item: &Item) -> bool {
        match item {
            Item::Binding(binding) => {
                if let Some(value) = &binding.value {
                    if self.is_borrow_producer_reference(value) {
                        self.diagnostics.push(Diagnostic::new(
                            value.syntax().span.clone(),
                            "a borrow-producing function cannot be stored or aliased",
                        ));
                        self.check_expression(value, false);
                    } else if let Some(origins) = self
                        .borrow_origins(value)
                        .or_else(|| self.direct_closure_borrow_origins(value))
                    {
                        self.check_expression(value, false);
                        if binding.mutable || self.function.is_none() {
                            self.diagnostics.push(Diagnostic::new(
                                value.syntax().span.clone(),
                                "a borrowed closure cannot escape its lexical scope",
                            ));
                        } else if let Some(symbol) = self.module.symbol_for(binding.syntax.id) {
                            for origin in &origins {
                                self.check_borrow_conflict(
                                    origin.source,
                                    origin.kind,
                                    value.syntax(),
                                );
                                self.active_borrows
                                    .entry(origin.source)
                                    .and_modify(|(kind, count)| {
                                        if *kind == BorrowKind::Shared
                                            && origin.kind == BorrowKind::Shared
                                        {
                                            *count += 1;
                                        }
                                    })
                                    .or_insert((origin.kind, 1));
                            }
                            self.borrowed_closures.insert(symbol, origins);
                            if let Some(scope) = self.borrow_scopes.last_mut() {
                                scope.push(symbol);
                            }
                        }
                    } else {
                        self.check_expression(value, true);
                    }
                }
                if let Some(symbol) = self.module.symbol_for(binding.syntax.id) {
                    if self.module.resolved().requires_initialization_state(symbol)
                        && self
                            .module
                            .type_of_symbol(symbol)
                            .is_some_and(|ty| !self.module.is_copy_in_function(ty, self.function))
                    {
                        self.diagnostics.push(Diagnostic::new(
                            binding.syntax.span.clone(),
                            "recursive move-only bindings are not supported",
                        ));
                    }
                    self.states.insert(symbol, ValueState::Available);
                }
                true
            }
            Item::PatternBinding(binding) => {
                self.check_expression(&binding.value, true);
                self.bind_pattern(&binding.pattern, false);
                true
            }
            Item::Assignment(assignment) => {
                if let Expression::Index(index) = &assignment.target {
                    self.check_expression(&index.value, false);
                    self.check_expression(&index.index, true);
                    self.check_expression(&assignment.value, true);
                    return true;
                }
                self.reject_borrowed_escape(&assignment.value);
                self.check_assignment_target(&assignment.target);
                self.check_expression(&assignment.value, true);
                if let Some(symbol) = self.module.symbol_for(assignment.target.syntax().id) {
                    self.states.insert(symbol, ValueState::Available);
                }
                true
            }
            Item::Return(item) => {
                self.reject_borrowed_escape(&item.value);
                self.check_expression(&item.value, true);
                false
            }
            Item::Break(item) => {
                if let Some(value) = &item.value {
                    self.reject_borrowed_escape(value);
                    self.check_expression(value, true);
                }
                if let Some(loop_) = self.loops.last_mut() {
                    loop_.breaks.push(self.states.clone());
                }
                false
            }
            Item::Continue(_) => {
                if let Some(loop_) = self.loops.last_mut() {
                    loop_.back_edges.push(self.states.clone());
                }
                false
            }
            Item::Expression(expression) => {
                self.reject_borrowed_escape(expression);
                self.check_expression(expression, true)
            }
            Item::Submodule(_) => true,
            Item::TypeDeclaration(_) => true,
            Item::UseDeclaration(_) => true,
            _ => true,
        }
    }

    /// Binds a function's parameter pattern, applying the implicit-borrow
    /// default to each top-level parameter position (the whole pattern, or a
    /// direct element of a top-level product — exactly the positions `mut`
    /// and `move` markers can address). A position freezes when it is
    /// `drop_method`'s `self` (existing, always frozen) or when it is an
    /// ordinary, non-`Copy` position that isn't covered by a `mut` marker
    /// (a mutable borrow) or a `move` marker (ownership transfer). Any other
    /// pattern shape at a position (e.g. a `Nominal` destructured directly at
    /// the parameter level) has no representable symbol to grant `mut`/`move`
    /// to, so it keeps today's behavior — that's `bind_pattern`'s existing
    /// recursive freeze propagation, unaffected by the new default.
    fn bind_function_pattern(&mut self, pattern: &Pattern, drop_method: bool) {
        if let Pattern::Product(product) = pattern {
            for element in &product.elements {
                let freeze = drop_method || self.parameter_element_freeze(element);
                self.bind_pattern(element, freeze);
            }
        } else {
            let freeze = drop_method || self.parameter_element_freeze(pattern);
            self.bind_pattern(pattern, freeze);
        }
    }

    fn parameter_element_freeze(&self, pattern: &Pattern) -> bool {
        let symbol = match pattern {
            Pattern::Binding(binding) => self.module.symbol_for(binding.syntax.id),
            Pattern::At(at) => self.module.symbol_for(at.binding.syntax.id),
            _ => None,
        };
        let Some(symbol) = symbol else {
            return false;
        };
        let Some(value_type) = self.module.type_of_symbol(symbol) else {
            return false;
        };
        !self.module.is_copy_in_function(value_type, self.function)
            && !self.module.has_mutable_storage(symbol)
            && !self.module.is_move_parameter(symbol)
    }

    fn bind_pattern(&mut self, pattern: &Pattern, freeze: bool) {
        match pattern {
            Pattern::Binding(value) => {
                let Some(symbol) = self.module.symbol_for(value.syntax.id) else {
                    return;
                };
                let frozen = freeze
                    && self
                        .module
                        .type_of_symbol(symbol)
                        .is_some_and(|ty| !self.module.is_copy_in_function(ty, self.function));
                if frozen && value.mutable {
                    self.diagnostics.push(Diagnostic::new(
                        value.syntax.span.clone(),
                        "a move-only value borrowed through `Ref` cannot be bound as `mut`",
                    ));
                }
                self.states.insert(
                    symbol,
                    if frozen {
                        ValueState::Frozen
                    } else {
                        ValueState::Available
                    },
                );
                if frozen {
                    self.info.non_owning_symbols.insert(symbol);
                }
            }
            Pattern::At(at) => {
                self.bind_pattern(&Pattern::Binding(at.binding.as_ref().clone()), freeze);
                self.bind_pattern(&at.pattern, freeze);
            }
            Pattern::Product(value) => {
                for element in &value.elements {
                    self.bind_pattern(element, freeze);
                }
            }
            Pattern::Nominal(value) => {
                let dereferences_ref = matches!(
                    self.module.type_of_pattern(value.syntax.id),
                    Some(crate::CheckedType::Ref(_))
                );
                self.bind_pattern(&value.argument, freeze || dereferences_ref);
            }
            Pattern::Wildcard(_) | Pattern::StringLiteral(_) | Pattern::Splice(_) => {}
        }
    }

    fn check_assignment_target(&mut self, expression: &Expression) {
        if let Some(symbol) = self.module.symbol_for(expression.syntax().id) {
            self.check_borrow_conflict(symbol, BorrowKind::Mutable, expression.syntax());
            return;
        }
        match expression {
            Expression::Name(_) => {}
            Expression::Access(access) => {
                self.check_expression(&access.value, false);
            }
            Expression::Index(index) => {
                self.check_expression(&index.value, false);
                self.check_expression(&index.index, true);
            }
            _ => {
                self.check_expression(expression, false);
            }
        }
    }

    fn use_symbol(&mut self, symbol: SymbolId, syntax: &Syntax, consume: bool) {
        let Some(value_type) = self.module.type_of_symbol(symbol) else {
            return;
        };
        if let Some((kind, _)) = self.active_borrows.get(&symbol).copied()
            && (consume || kind == BorrowKind::Mutable)
        {
            self.diagnostics.push(Diagnostic::new(
                syntax.span.clone(),
                match kind {
                    BorrowKind::Shared => "cannot move a value while it is borrowed by a closure",
                    BorrowKind::Mutable => {
                        "cannot access a value while it is mutably borrowed by a closure"
                    }
                },
            ));
            return;
        }
        if self.module.is_copy_in_function(value_type, self.function) {
            return;
        }

        if self.top_level_symbols.contains(&symbol) {
            if consume {
                self.diagnostics.push(Diagnostic::new(
                    syntax.span.clone(),
                    "cannot move a value out of a global binding",
                ));
            }
            return;
        }

        let Some(state) = self.states.get(&symbol).copied() else {
            // A non-local, non-global symbol is a capture, seeded when
            // checking its owning function.
            return;
        };
        match state {
            ValueState::Moved => self
                .diagnostics
                .push(Diagnostic::new(syntax.span.clone(), "use of moved value")),
            ValueState::MaybeMoved => self.diagnostics.push(Diagnostic::new(
                syntax.span.clone(),
                "value may have been moved on another control-flow path",
            )),
            ValueState::Frozen if consume => self.diagnostics.push(Diagnostic::new(
                syntax.span.clone(),
                "cannot move out of a borrowed value",
            )),
            ValueState::Available if consume => {
                self.states.insert(symbol, ValueState::Moved);
                self.info
                    .moved_uses
                    .entry(syntax.id)
                    .or_default()
                    .insert(symbol);
            }
            ValueState::Available | ValueState::Frozen => {}
        }
    }
}

fn infer_borrow_summaries(module: &TypedModule) -> HashMap<FunctionId, BorrowResultSummary> {
    let mut summaries = HashMap::new();
    for function in module.functions() {
        let parameters = top_level_parameter_symbols(module, &function.pattern);
        let Some(returned) = returned_closure_ids(module, &function.body) else {
            continue;
        };
        let mut common: Option<Vec<(usize, BorrowKind)>> = None;
        for closure in returned {
            let Some(closure) = module.function_by_id(closure) else {
                common = None;
                break;
            };
            let mut borrowed = vec![];
            for (index, parameter) in parameters.iter().enumerate() {
                let Some(parameter) = parameter else {
                    continue;
                };
                let Some(parameter_type) = module.type_of_symbol(*parameter) else {
                    continue;
                };
                if closure.captures.contains(parameter)
                    && !module.is_move_parameter(*parameter)
                    && !module.is_copy_in_function(parameter_type, Some(function.id))
                {
                    borrowed.push((
                        index,
                        if module.is_mutated_parameter(*parameter) {
                            BorrowKind::Mutable
                        } else {
                            BorrowKind::Shared
                        },
                    ));
                }
            }
            match &common {
                None => common = Some(borrowed),
                Some(previous) if previous == &borrowed => {}
                Some(_) => {
                    common = None;
                    break;
                }
            }
        }
        if let Some(parameters) = common.filter(|values| !values.is_empty()) {
            summaries.insert(function.id, BorrowResultSummary { parameters });
        }
    }
    summaries
}

fn returned_closure_ids(module: &TypedModule, expression: &Expression) -> Option<Vec<FunctionId>> {
    match expression {
        Expression::Function(function) => module
            .function_for(function.syntax.id)
            .map(|function| vec![function]),
        Expression::Satisfies(value) => returned_closure_ids(module, &value.value),
        Expression::Block(block) => match block.items.last()? {
            Item::Expression(expression) => returned_closure_ids(module, expression),
            Item::Return(value) => returned_closure_ids(module, &value.value),
            _ => None,
        },
        Expression::Match(value) => {
            let mut result = vec![];
            for arm in &value.arms {
                result.extend(returned_closure_ids(module, &arm.body)?);
            }
            Some(result)
        }
        _ => None,
    }
}

fn top_level_parameter_symbols(module: &TypedModule, pattern: &Pattern) -> Vec<Option<SymbolId>> {
    let symbol = |pattern: &Pattern| match pattern {
        Pattern::Binding(binding) => module.symbol_for(binding.syntax.id),
        Pattern::At(at) => module.symbol_for(at.binding.syntax.id),
        _ => None,
    };
    match pattern {
        Pattern::Product(product) => product.elements.iter().map(symbol).collect(),
        _ => vec![symbol(pattern)],
    }
}

fn call_argument_at(expression: &Expression, position: usize) -> Option<&Expression> {
    match expression {
        Expression::Product(product) => product.elements.get(position).map(|element| &element.value),
        _ if position == 0 => Some(expression),
        _ => None,
    }
}

fn merge_states(
    outer: &HashMap<SymbolId, ValueState>,
    branches: &[HashMap<SymbolId, ValueState>],
) -> HashMap<SymbolId, ValueState> {
    if branches.is_empty() {
        return outer.clone();
    }
    let mut result = outer.clone();
    for symbol in outer.keys() {
        let first = branches[0]
            .get(symbol)
            .copied()
            .unwrap_or(ValueState::Available);
        let same = branches
            .iter()
            .all(|branch| branch.get(symbol).copied().unwrap_or(ValueState::Available) == first);
        result.insert(
            *symbol,
            if same {
                first
            } else if first == ValueState::Frozen {
                ValueState::Frozen
            } else {
                ValueState::MaybeMoved
            },
        );
    }
    result
}
