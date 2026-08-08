use std::collections::HashMap;

use crate::{
    Binding, BindingKind, BlockExpression, Diagnostic, Expression, Item, Module, Parameter, Span,
    Statement, SyntaxId, Type,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SymbolId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FunctionId(pub usize);

#[derive(Debug, Clone)]
pub struct ResolvedFunction {
    pub id: FunctionId,
    pub name: String,
    pub binding_syntax: Option<SyntaxId>,
    pub parameter: Parameter,
    pub return_annotation: Option<Type>,
    pub binding_annotation: Option<Type>,
    pub body: Expression,
}

#[derive(Debug, Clone)]
pub struct ResolvedModule {
    module: Module,
    functions: Vec<ResolvedFunction>,
    symbols: HashMap<SyntaxId, SymbolId>,
    function_expressions: HashMap<SyntaxId, FunctionId>,
}

impl ResolvedModule {
    pub fn syntax(&self) -> &Module {
        &self.module
    }

    pub fn functions(&self) -> &[ResolvedFunction] {
        &self.functions
    }

    pub fn symbol_for(&self, syntax_id: SyntaxId) -> Option<SymbolId> {
        self.symbols.get(&syntax_id).copied()
    }

    pub fn function_for(&self, syntax_id: SyntaxId) -> Option<FunctionId> {
        self.function_expressions.get(&syntax_id).copied()
    }
}

#[derive(Default)]
pub struct NameResolver {
    scopes: Vec<HashMap<String, SymbolId>>,
    symbols: HashMap<SyntaxId, SymbolId>,
    function_expressions: HashMap<SyntaxId, FunctionId>,
    functions: Vec<ResolvedFunction>,
    diagnostics: Vec<Diagnostic>,
    next_symbol_id: usize,
    next_function_id: usize,
}

impl NameResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn resolve(mut self, module: &Module) -> Result<ResolvedModule, Vec<Diagnostic>> {
        self.push_scope();
        self.predeclare_items(&module.items);
        for item in &module.items {
            self.resolve_item(item);
        }
        self.pop_scope();

        if !self.diagnostics.is_empty() {
            return Err(self.diagnostics);
        }

        self.functions.sort_by_key(|function| function.id.0);
        Ok(ResolvedModule {
            module: module.clone(),
            functions: self.functions,
            symbols: self.symbols,
            function_expressions: self.function_expressions,
        })
    }

    fn predeclare_items(&mut self, items: &[Item]) {
        for item in items {
            match item {
                Item::ExternBlock(block) => {
                    for binding in &block.bindings {
                        self.declare_binding(binding);
                    }
                }
                Item::Statement(statement) => {
                    if let Statement::Binding(binding) = statement.as_ref()
                        && binding.kind == BindingKind::Def
                    {
                        self.declare_binding(binding);
                    }
                }
                Item::TypeDeclaration(_) => {}
            }
        }
    }

    fn resolve_item(&mut self, item: &Item) {
        match item {
            Item::ExternBlock(_) | Item::TypeDeclaration(_) => {}
            Item::Statement(statement) => self.resolve_statement(statement),
        }
    }

    fn resolve_statement(&mut self, statement: &Statement) {
        match statement {
            Statement::Binding(binding) => self.resolve_binding(binding),
            Statement::Expression(expression) => self.resolve_expression(expression, None, None),
        }
    }

    fn resolve_binding(&mut self, binding: &Binding) {
        if let Some(value) = &binding.value {
            self.resolve_expression(
                value,
                binding.annotation.as_ref(),
                Some((&binding.name, binding.syntax.id)),
            );
        }
        if binding.kind == BindingKind::Let {
            self.declare_binding(binding);
        }
    }

    fn resolve_expression(
        &mut self,
        expression: &Expression,
        expected_type: Option<&Type>,
        suggested_function: Option<(&str, SyntaxId)>,
    ) {
        match expression {
            Expression::Function(function) => {
                let function_id = FunctionId(self.next_function_id);
                self.next_function_id += 1;
                self.function_expressions
                    .insert(function.syntax.id, function_id);

                self.push_scope();
                self.declare_parameter(&function.parameter);
                self.resolve_expression(&function.body, None, None);
                self.pop_scope();

                self.functions.push(ResolvedFunction {
                    id: function_id,
                    name: suggested_function
                        .map(|(name, _)| name.to_owned())
                        .unwrap_or_else(|| format!("function.{}", function_id.0)),
                    binding_syntax: suggested_function.map(|(_, syntax_id)| syntax_id),
                    parameter: function.parameter.clone(),
                    return_annotation: function.return_type.clone(),
                    binding_annotation: expected_type.cloned(),
                    body: (*function.body).clone(),
                });
            }
            Expression::Block(block) => self.resolve_block(block),
            Expression::Product(product) => {
                for element in &product.elements {
                    self.resolve_expression(&element.value, None, None);
                }
            }
            Expression::Call(call) => {
                self.resolve_expression(&call.callee, None, None);
                self.resolve_expression(&call.argument, None, None);
            }
            Expression::Access(access) => self.resolve_expression(&access.value, None, None),
            Expression::Binary(binary) => {
                self.resolve_expression(&binary.left, None, None);
                self.resolve_expression(&binary.right, None, None);
            }
            Expression::Name(name) => match self.lookup(&name.name) {
                Some(symbol) => {
                    self.symbols.insert(name.syntax.id, symbol);
                }
                None => self.diagnostics.push(Diagnostic::new(
                    name.syntax.span.clone(),
                    format!("unknown name `{}`", name.name),
                )),
            },
            Expression::String(_) | Expression::Integer(_) => {}
        }
    }

    fn resolve_block(&mut self, block: &BlockExpression) {
        self.push_scope();
        for statement in &block.statements {
            if let Statement::Binding(binding) = statement
                && binding.kind == BindingKind::Def
            {
                self.declare_binding(binding);
            }
        }
        for statement in &block.statements {
            self.resolve_statement(statement);
        }
        self.pop_scope();
    }

    fn declare_parameter(&mut self, parameter: &Parameter) {
        match parameter {
            Parameter::Value(value) => {
                self.declare(&value.name, value.syntax.id, value.syntax.span.clone());
            }
            Parameter::Product(product) => {
                for value in &product.elements {
                    self.declare(&value.name, value.syntax.id, value.syntax.span.clone());
                }
            }
        }
    }

    fn declare_binding(&mut self, binding: &Binding) {
        self.declare(
            &binding.name,
            binding.syntax.id,
            binding.syntax.span.clone(),
        );
    }

    fn declare(&mut self, name: &str, syntax_id: SyntaxId, span: Span) {
        let scope = self.scopes.last_mut().expect("resolver always has a scope");
        if scope.contains_key(name) {
            self.diagnostics.push(Diagnostic::new(
                span,
                format!("duplicate definition of `{name}`"),
            ));
            return;
        }
        let symbol = SymbolId(self.next_symbol_id);
        self.next_symbol_id += 1;
        scope.insert(name.to_owned(), symbol);
        self.symbols.insert(syntax_id, symbol);
    }

    fn lookup(&self, name: &str) -> Option<SymbolId> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }
}
