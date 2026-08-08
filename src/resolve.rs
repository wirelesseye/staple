use std::collections::HashMap;

use crate::{
    Binding, BindingKind, BlockExpression, Diagnostic, Expression, Item, Module, ModuleId, Pattern,
    Program, Span, Statement, SyntaxId, Type, TypeDeclaration, UseKind, Visibility,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SymbolId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FunctionId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeId(pub usize);

#[derive(Debug, Clone)]
pub struct ResolvedFunction {
    pub id: FunctionId,
    pub name: String,
    pub binding_syntax: Option<SyntaxId>,
    pub pattern: Pattern,
    pub return_annotation: Option<Type>,
    pub binding_annotation: Option<Type>,
    pub captures: Vec<SymbolId>,
    pub body: Expression,
}

#[derive(Debug, Clone)]
pub struct ResolvedModule {
    program: Program,
    functions: Vec<ResolvedFunction>,
    symbols: HashMap<SyntaxId, SymbolId>,
    function_expressions: HashMap<SyntaxId, FunctionId>,
    named_types: HashMap<SyntaxId, TypeId>,
    type_declarations: HashMap<TypeId, TypeDeclaration>,
    type_names: HashMap<TypeId, String>,
    syntax_modules: HashMap<SyntaxId, ModuleId>,
}

impl ResolvedModule {
    pub fn syntax(&self) -> &Module {
        &self.program.module(self.program.entry()).syntax
    }

    pub fn program(&self) -> &Program {
        &self.program
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

    pub fn type_for(&self, syntax_id: SyntaxId) -> Option<TypeId> {
        self.named_types.get(&syntax_id).copied()
    }

    pub fn type_declarations(&self) -> &HashMap<TypeId, TypeDeclaration> {
        &self.type_declarations
    }

    pub fn type_name(&self, id: TypeId) -> Option<&str> {
        self.type_names.get(&id).map(String::as_str)
    }

    pub fn module_for_syntax(&self, syntax_id: SyntaxId) -> Option<ModuleId> {
        self.syntax_modules.get(&syntax_id).copied()
    }
}

#[derive(Default)]
struct Interface {
    values: HashMap<String, SymbolId>,
    types: HashMap<String, TypeId>,
}

#[derive(Default)]
pub struct NameResolver {
    scopes: Vec<HashMap<String, SymbolId>>,
    namespaces: HashMap<String, ModuleId>,
    imported_types: HashMap<String, TypeId>,
    symbols: HashMap<SyntaxId, SymbolId>,
    function_expressions: HashMap<SyntaxId, FunctionId>,
    symbol_owners: HashMap<SymbolId, Option<FunctionId>>,
    function_parents: HashMap<FunctionId, Option<FunctionId>>,
    function_captures: HashMap<FunctionId, Vec<SymbolId>>,
    function_stack: Vec<FunctionId>,
    functions: Vec<ResolvedFunction>,
    named_types: HashMap<SyntaxId, TypeId>,
    type_declarations: HashMap<TypeId, TypeDeclaration>,
    type_names: HashMap<TypeId, String>,
    syntax_modules: HashMap<SyntaxId, ModuleId>,
    interfaces: Vec<Interface>,
    declared_symbols: HashMap<SyntaxId, SymbolId>,
    declared_types: Vec<HashMap<String, TypeId>>,
    diagnostics: Vec<Diagnostic>,
    next_symbol_id: usize,
    next_function_id: usize,
    current_module: ModuleId,
    multiple_modules: bool,
}

impl NameResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn resolve(self, module: &Module) -> Result<ResolvedModule, Vec<Diagnostic>> {
        self.resolve_program(Program::single(module.clone()))
    }

    pub fn resolve_program(mut self, program: Program) -> Result<ResolvedModule, Vec<Diagnostic>> {
        self.multiple_modules = program.modules().len() > 1;
        self.collect_interfaces(&program);
        for source_module in program.modules() {
            self.current_module = source_module.id;
            self.scopes.clear();
            self.namespaces.clear();
            self.imported_types.clear();
            self.push_scope();
            self.install_imports(&program, source_module.id);
            self.predeclare_items(&source_module.syntax.items);
            for item in &source_module.syntax.items {
                self.resolve_item(item);
            }
            self.pop_scope();
        }

        if !self.diagnostics.is_empty() {
            return Err(self.diagnostics);
        }
        self.functions.sort_by_key(|function| function.id.0);
        Ok(ResolvedModule {
            program,
            functions: self.functions,
            symbols: self.symbols,
            function_expressions: self.function_expressions,
            named_types: self.named_types,
            type_declarations: self.type_declarations,
            type_names: self.type_names,
            syntax_modules: self.syntax_modules,
        })
    }

    fn collect_interfaces(&mut self, program: &Program) {
        self.interfaces = (0..program.modules().len())
            .map(|_| Interface::default())
            .collect();
        self.declared_types = (0..program.modules().len())
            .map(|_| HashMap::new())
            .collect();
        for source_module in program.modules() {
            for item in &source_module.syntax.items {
                match item {
                    Item::ExternBlock(block) => {
                        for binding in &block.bindings {
                            let symbol = self.allocate_symbol(binding);
                            if block.visibility == Visibility::Public {
                                self.insert_public_value(
                                    source_module.id,
                                    &binding.name,
                                    symbol,
                                    binding.syntax.span.clone(),
                                );
                            }
                        }
                    }
                    Item::TypeDeclaration(declaration) => {
                        let id = TypeId(self.type_declarations.len());
                        if self.declared_types[source_module.id.0]
                            .insert(declaration.name.clone(), id)
                            .is_some()
                        {
                            self.diagnostics.push(Diagnostic::new(
                                declaration.syntax.span.clone(),
                                format!("duplicate type definition of `{}`", declaration.name),
                            ));
                        }
                        self.type_declarations.insert(id, declaration.clone());
                        let qualified = if self.multiple_modules {
                            format!("m{}.{}", source_module.id.0, declaration.name)
                        } else {
                            declaration.name.clone()
                        };
                        self.type_names.insert(id, qualified);
                        if declaration.visibility == Visibility::Public {
                            self.interfaces[source_module.id.0]
                                .types
                                .insert(declaration.name.clone(), id);
                        }
                    }
                    Item::Statement(statement) => {
                        if let Statement::Binding(binding) = statement.as_ref() {
                            let symbol = self.allocate_symbol(binding);
                            if binding.visibility == Visibility::Public {
                                self.insert_public_value(
                                    source_module.id,
                                    &binding.name,
                                    symbol,
                                    binding.syntax.span.clone(),
                                );
                            }
                        }
                    }
                    Item::UseDeclaration(_) => {}
                }
            }
        }
    }

    fn allocate_symbol(&mut self, binding: &Binding) -> SymbolId {
        let symbol = SymbolId(self.next_symbol_id);
        self.next_symbol_id += 1;
        self.declared_symbols.insert(binding.syntax.id, symbol);
        self.symbols.insert(binding.syntax.id, symbol);
        self.symbol_owners.insert(symbol, None);
        symbol
    }

    fn insert_public_value(&mut self, module: ModuleId, name: &str, symbol: SymbolId, span: Span) {
        if self.interfaces[module.0]
            .values
            .insert(name.to_owned(), symbol)
            .is_some()
        {
            self.diagnostics.push(Diagnostic::new(
                span,
                format!("duplicate definition of `{name}`"),
            ));
        }
    }

    fn install_imports(&mut self, program: &Program, module: ModuleId) {
        for item in &program.module(module).syntax.items {
            let Item::UseDeclaration(declaration) = item else {
                continue;
            };
            let Some(imported) = program.imported_module(declaration.syntax.id) else {
                continue;
            };
            match &declaration.kind {
                UseKind::Namespace => {
                    let name = declaration
                        .path
                        .last()
                        .expect("use path is nonempty")
                        .clone();
                    if self.namespaces.insert(name.clone(), imported).is_some()
                        || self.current_scope().contains_key(&name)
                    {
                        self.duplicate_import(&name, declaration.syntax.span.clone());
                    }
                }
                UseKind::Glob => {
                    for (name, symbol) in self.interfaces[imported.0].values.clone() {
                        self.insert_imported_value(name, symbol, declaration.syntax.span.clone());
                    }
                    for (name, ty) in self.interfaces[imported.0].types.clone() {
                        self.insert_imported_type(name, ty, declaration.syntax.span.clone());
                    }
                }
                UseKind::Selected(names) => {
                    for name in names {
                        self.install_selected(
                            imported,
                            name,
                            name,
                            declaration.syntax.span.clone(),
                        );
                    }
                }
                UseKind::Renamed { item, alias } => {
                    self.install_selected(imported, item, alias, declaration.syntax.span.clone());
                }
            }
        }
    }

    fn install_selected(&mut self, module: ModuleId, item: &str, local: &str, span: Span) {
        let mut found = false;
        if let Some(symbol) = self.interfaces[module.0].values.get(item).copied() {
            found = true;
            self.insert_imported_value(local.to_owned(), symbol, span.clone());
        }
        if let Some(ty) = self.interfaces[module.0].types.get(item).copied() {
            found = true;
            self.insert_imported_type(local.to_owned(), ty, span.clone());
        }
        if !found {
            self.diagnostics.push(Diagnostic::new(
                span,
                format!("module has no public item named `{item}`"),
            ));
        }
    }

    fn insert_imported_value(&mut self, name: String, symbol: SymbolId, span: Span) {
        if self.current_scope().contains_key(&name) || self.namespaces.contains_key(&name) {
            self.duplicate_import(&name, span);
        } else {
            self.current_scope_mut().insert(name, symbol);
        }
    }

    fn insert_imported_type(&mut self, name: String, ty: TypeId, span: Span) {
        if self.declared_types[self.current_module.0].contains_key(&name)
            || self.imported_types.insert(name.clone(), ty).is_some()
        {
            self.duplicate_import(&name, span);
        }
    }

    fn duplicate_import(&mut self, name: &str, span: Span) {
        self.diagnostics.push(Diagnostic::new(
            span,
            format!("duplicate import of `{name}`"),
        ));
    }

    fn predeclare_items(&mut self, items: &[Item]) {
        for item in items {
            match item {
                Item::ExternBlock(block) => {
                    for binding in &block.bindings {
                        self.declare_allocated(binding);
                    }
                }
                Item::Statement(statement) => {
                    if let Statement::Binding(binding) = statement.as_ref()
                        && binding.kind == BindingKind::Def
                    {
                        self.declare_allocated(binding);
                    }
                }
                Item::UseDeclaration(_) | Item::TypeDeclaration(_) => {}
            }
        }
    }

    fn resolve_item(&mut self, item: &Item) {
        match item {
            Item::UseDeclaration(_) => {}
            Item::ExternBlock(block) => {
                for binding in &block.bindings {
                    if let Some(annotation) = &binding.annotation {
                        self.resolve_type(annotation);
                    }
                }
            }
            Item::TypeDeclaration(declaration) => self.resolve_type(&declaration.underlying),
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
        if let Some(annotation) = &binding.annotation {
            self.resolve_type(annotation);
        }
        if let Some(value) = &binding.value {
            self.resolve_expression(
                value,
                binding.annotation.as_ref(),
                Some((&binding.name, binding.syntax.id)),
            );
        }
        if binding.kind == BindingKind::Let {
            self.declare_allocated(binding);
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
                self.function_parents
                    .insert(function_id, self.function_stack.last().copied());
                self.resolve_type(&function.pattern.ty());
                if let Some(ty) = &function.return_type {
                    self.resolve_type(ty);
                }
                self.function_stack.push(function_id);
                self.push_scope();
                self.declare_pattern(&function.pattern);
                let expected_result = function.return_type.as_ref().or_else(|| {
                    let Type::Function(function_type) = expected_type? else {
                        return None;
                    };
                    Some(function_type.result.as_ref())
                });
                self.resolve_expression(&function.body, expected_result, None);
                self.pop_scope();
                self.function_stack.pop();
                let base_name = suggested_function
                    .map(|(name, _)| name.to_owned())
                    .unwrap_or_else(|| format!("function.{}", function_id.0));
                let name = if self.multiple_modules || base_name == "main" {
                    format!("__staple_m{}_{}", self.current_module.0, base_name)
                } else {
                    base_name
                };
                let mut captures = self
                    .function_captures
                    .remove(&function_id)
                    .unwrap_or_default();
                if let Some((_, binding_syntax)) = suggested_function
                    && let Some(self_symbol) = self.symbols.get(&binding_syntax)
                {
                    captures.retain(|capture| capture != self_symbol);
                }
                self.functions.push(ResolvedFunction {
                    id: function_id,
                    name,
                    binding_syntax: suggested_function.map(|(_, syntax)| syntax),
                    pattern: function.pattern.clone(),
                    return_annotation: function.return_type.clone(),
                    binding_annotation: expected_type.cloned(),
                    captures,
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
            Expression::Access(access) => {
                if let Expression::Name(namespace) = access.value.as_ref()
                    && let crate::Accessor::Name(item) = &access.accessor
                    && let Some(module) = self.namespaces.get(&namespace.name).copied()
                {
                    if let Some(symbol) = self.interfaces[module.0].values.get(item).copied() {
                        self.symbols.insert(access.syntax.id, symbol);
                    } else {
                        self.diagnostics.push(Diagnostic::new(
                            access.syntax.span.clone(),
                            format!(
                                "module `{}` has no public value named `{item}`",
                                namespace.name
                            ),
                        ));
                    }
                } else {
                    self.resolve_expression(&access.value, None, None);
                }
            }
            Expression::Binary(binary) => {
                self.resolve_expression(&binary.left, None, None);
                self.resolve_expression(&binary.right, None, None);
            }
            Expression::Name(name) => match self.lookup(&name.name) {
                Some(symbol) => {
                    self.symbols.insert(name.syntax.id, symbol);
                    self.record_capture(symbol);
                }
                None if self.namespaces.contains_key(&name.name) => {
                    self.diagnostics.push(Diagnostic::new(
                        name.syntax.span.clone(),
                        format!("module namespace `{}` is not a value", name.name),
                    ))
                }
                None => self.diagnostics.push(Diagnostic::new(
                    name.syntax.span.clone(),
                    format!("unknown name `{}`", name.name),
                )),
            },
            Expression::String(_) | Expression::Integer(_) => {}
        }
    }

    fn resolve_type(&mut self, ty: &Type) {
        match ty {
            Type::Named(named) => {
                let resolved = if let Some(namespace) = &named.namespace {
                    self.namespaces
                        .get(namespace)
                        .and_then(|module| self.interfaces[module.0].types.get(&named.name))
                        .copied()
                } else {
                    self.declared_types[self.current_module.0]
                        .get(&named.name)
                        .copied()
                        .or_else(|| self.imported_types.get(&named.name).copied())
                };
                if let Some(id) = resolved {
                    self.named_types.insert(named.syntax.id, id);
                } else if !matches!(named.name.as_str(), "int" | "string" | "c_char") {
                    self.diagnostics.push(Diagnostic::new(
                        named.syntax.span.clone(),
                        format!("unknown type `{}`", named.name),
                    ));
                }
            }
            Type::Pointer(pointer) => self.resolve_type(&pointer.pointee),
            Type::Product(product) => {
                for element in &product.elements {
                    self.resolve_type(&element.ty);
                }
            }
            Type::Function(function) => {
                self.resolve_type(&function.parameter);
                self.resolve_type(&function.result);
            }
            Type::Inferred(_) | Type::Primitive(_) => {}
        }
    }

    fn resolve_block(&mut self, block: &BlockExpression) {
        self.push_scope();
        for statement in &block.statements {
            if let Statement::Binding(binding) = statement
                && binding.kind == BindingKind::Def
            {
                self.declare_fresh(binding);
            }
        }
        for statement in &block.statements {
            self.resolve_statement(statement);
        }
        self.pop_scope();
    }

    fn declare_pattern(&mut self, pattern: &Pattern) {
        match pattern {
            Pattern::Binding(binding) => self.declare_fresh_name(
                &binding.name,
                binding.syntax.id,
                binding.syntax.span.clone(),
            ),
            Pattern::Product(product) => {
                for element in &product.elements {
                    self.declare_pattern(element);
                }
            }
        }
    }

    fn declare_allocated(&mut self, binding: &Binding) {
        if let Some(symbol) = self.declared_symbols.get(&binding.syntax.id).copied() {
            self.declare_symbol(
                &binding.name,
                binding.syntax.id,
                binding.syntax.span.clone(),
                symbol,
            );
        } else {
            self.declare_fresh(binding);
        }
    }

    fn declare_fresh(&mut self, binding: &Binding) {
        self.declare_fresh_name(
            &binding.name,
            binding.syntax.id,
            binding.syntax.span.clone(),
        );
    }

    fn declare_fresh_name(&mut self, name: &str, syntax: SyntaxId, span: Span) {
        let symbol = SymbolId(self.next_symbol_id);
        self.next_symbol_id += 1;
        self.symbol_owners
            .insert(symbol, self.function_stack.last().copied());
        self.declare_symbol(name, syntax, span, symbol);
    }

    fn record_capture(&mut self, symbol: SymbolId) {
        let Some(mut function) = self.function_stack.last().copied() else {
            return;
        };
        let Some(owner) = self.symbol_owners.get(&symbol).copied().flatten() else {
            return;
        };
        while function != owner {
            let captures = self.function_captures.entry(function).or_default();
            if !captures.contains(&symbol) {
                captures.push(symbol);
            }
            let Some(Some(parent)) = self.function_parents.get(&function).copied() else {
                break;
            };
            function = parent;
        }
    }

    fn declare_symbol(&mut self, name: &str, syntax: SyntaxId, span: Span, symbol: SymbolId) {
        if self.current_scope().contains_key(name) || self.namespaces.contains_key(name) {
            self.diagnostics.push(Diagnostic::new(
                span,
                format!("duplicate definition of `{name}`"),
            ));
            return;
        }
        self.current_scope_mut().insert(name.to_owned(), symbol);
        self.symbols.insert(syntax, symbol);
        self.syntax_modules.insert(syntax, self.current_module);
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
    fn current_scope(&self) -> &HashMap<String, SymbolId> {
        self.scopes.last().expect("resolver scope")
    }
    fn current_scope_mut(&mut self) -> &mut HashMap<String, SymbolId> {
        self.scopes.last_mut().expect("resolver scope")
    }
}
