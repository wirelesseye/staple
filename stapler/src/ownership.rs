use std::collections::{HashMap, HashSet};

use crate::{
    Diagnostic, Expression, FunctionId, Item, Pattern, ResolvedFunction, Statement, SymbolId,
    Syntax, SyntaxId, TypedModule,
};

#[derive(Debug, Clone, Default)]
pub(crate) struct OwnershipInfo {
    moved_uses: HashMap<SyntaxId, HashSet<SymbolId>>,
    non_owning_symbols: HashSet<SymbolId>,
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
    info: OwnershipInfo,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> OwnershipChecker<'a> {
    pub(crate) fn check(module: &'a TypedModule) -> (OwnershipInfo, Vec<Diagnostic>) {
        let mut checker = Self {
            module,
            function: None,
            states: HashMap::new(),
            info: OwnershipInfo::default(),
            diagnostics: vec![],
        };
        checker.check_globals();

        for function in module.functions() {
            checker.check_function(function);
        }

        (checker.info, checker.diagnostics)
    }

    fn check_globals(&mut self) {
        for source_module in self.module.resolved().program().modules() {
            for item in &source_module.syntax.items {
                let Item::Statement(statement) = item else {
                    continue;
                };
                match statement.as_ref() {
                    Statement::Binding(binding) if binding.value.is_some() => {
                        let Some(symbol) = self.module.symbol_for(binding.syntax.id) else {
                            continue;
                        };
                        let Some(value_type) = self.module.type_of_symbol(symbol) else {
                            continue;
                        };
                        if !self.module.is_copy_type(value_type) {
                            self.diagnostics.push(Diagnostic::new(
                                binding.syntax.span.clone(),
                                "move-only values cannot be stored in global bindings",
                            ));
                        }
                    }
                    Statement::PatternBinding(binding) => {
                        self.check_global_pattern(&binding.pattern);
                    }
                    _ => {}
                }
            }
        }
    }

    fn check_global_pattern(&mut self, pattern: &Pattern) {
        match pattern {
            Pattern::Binding(binding) => {
                let Some(symbol) = self.module.symbol_for(binding.syntax.id) else {
                    return;
                };
                if self
                    .module
                    .type_of_symbol(symbol)
                    .is_some_and(|ty| !self.module.is_copy_type(ty))
                {
                    self.diagnostics.push(Diagnostic::new(
                        binding.syntax.span.clone(),
                        "move-only values cannot be stored in global bindings",
                    ));
                }
            }
            Pattern::Product(product) => {
                for element in &product.elements {
                    self.check_global_pattern(element);
                }
            }
            Pattern::Nominal(nominal) => self.check_global_pattern(&nominal.argument),
            Pattern::Wildcard(_) | Pattern::StringLiteral(_) => {}
        }
    }

    fn check_function(&mut self, function: &ResolvedFunction) {
        self.function = Some(function.id);
        self.states.clear();

        let drop_method = self.module.is_drop_method(function.id);
        self.bind_pattern(&function.pattern, drop_method);
        for capture in &function.captures {
            let Some(value_type) = self.module.type_of_symbol(*capture) else {
                continue;
            };
            let state = if self
                .module
                .is_copy_in_function(value_type, Some(function.id))
                || self.module.resolved().is_mutable_symbol(*capture)
            {
                ValueState::Available
            } else {
                ValueState::Frozen
            };
            self.states.insert(*capture, state);
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
                        if !self.module.resolved().is_mutable_symbol(capture) {
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
            Expression::Block(value) => {
                for statement in &value.statements {
                    if !self.check_statement(statement) {
                        return false;
                    }
                }
                true
            }
            Expression::Product(value) => {
                for element in &value.elements {
                    self.check_expression(&element.value, true);
                }
                true
            }
            Expression::Call(value) => {
                self.check_expression(&value.callee, false);
                let scoped_c_string = self
                    .module
                    .symbol_for(value.callee.syntax().id)
                    .is_some_and(|symbol| self.module.resolved().is_external_symbol(symbol))
                    && self
                        .module
                        .type_of_expression(value.argument.syntax().id)
                        .is_some_and(|ty| matches!(ty, crate::CheckedType::CString));
                self.check_expression(&value.argument, !scoped_c_string);
                true
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
                let result_is_copy = self
                    .module
                    .type_of_expression(value.syntax.id)
                    .is_none_or(|ty| self.module.is_copy_in_function(ty, self.function));
                if consume && !result_is_copy {
                    self.diagnostics.push(Diagnostic::new(
                        value.syntax.span.clone(),
                        "cannot move an element out through an index; destructure the whole value",
                    ));
                }
                self.check_expression(&value.value, false);
                self.check_expression(&value.index, true);
                true
            }
            Expression::Infix(value) => {
                if let Some(lowered) = self.module.resolved().lowered_infix(value.syntax.id) {
                    self.check_expression(lowered, consume)
                } else {
                    for operand in &value.operands {
                        self.check_expression(operand, true);
                    }
                    true
                }
            }
            Expression::Name(value) => {
                if let Some(symbol) = self.module.symbol_for(value.syntax.id) {
                    self.use_symbol(symbol, &value.syntax, consume);
                }
                true
            }
            Expression::Quote(_) | Expression::Splice(_) => true,
            Expression::String(_) | Expression::CString(_) | Expression::Integer(_) => true,
        }
    }

    fn check_statement(&mut self, statement: &Statement) -> bool {
        match statement {
            Statement::Binding(binding) => {
                if let Some(value) = &binding.value {
                    self.check_expression(value, true);
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
            Statement::PatternBinding(binding) => {
                self.check_expression(&binding.value, true);
                self.bind_pattern(&binding.pattern, false);
                true
            }
            Statement::Assignment(assignment) => {
                self.check_assignment_target(&assignment.target);
                self.check_expression(&assignment.value, true);
                if let Some(symbol) = self.module.symbol_for(assignment.target.syntax().id) {
                    self.states.insert(symbol, ValueState::Available);
                }
                true
            }
            Statement::Return(statement) => {
                self.check_expression(&statement.value, true);
                false
            }
            Statement::Expression(expression) => self.check_expression(expression, true),
        }
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
                        "a move-only value borrowed through `Ref` cannot be bound as mutable",
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
            Pattern::Wildcard(_) | Pattern::StringLiteral(_) => {}
        }
    }

    fn check_assignment_target(&mut self, expression: &Expression) {
        if self.module.symbol_for(expression.syntax().id).is_some() {
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
        if self.module.is_copy_in_function(value_type, self.function) {
            return;
        }

        let Some(state) = self.states.get(&symbol).copied() else {
            // A non-local symbol is either a global (diagnosed separately) or a
            // capture. Captures are seeded when checking their function.
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
                "cannot move this value from a destructor or closure capture",
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
