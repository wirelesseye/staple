use std::collections::HashMap;

use crate::{
    AccessExpression, Accessor, Associativity, Binding, BindingKind, BlockExpression,
    CallExpression, Diagnostic, Expression, Fixity, InfixExpression, InfixOperator, Item, Module,
    ModuleId, NameExpression, Pattern, Program, Span, Statement, Syntax, SyntaxId, Type,
    TypeDeclaration, TypeParameterPattern, UseKind, Visibility,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SymbolId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FunctionId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeParameterId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MacroId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltinType {
    I32,
    Bool,
    String,
    CChar,
    CString,
    CPointer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrimitiveMacro {
    CString,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntrinsicFunction {
    I32Add,
    I32Subtract,
    I32Multiply,
    I32Divide,
    StringFromCString,
    StringToCString,
}

#[derive(Debug, Clone)]
pub struct ResolvedFunction {
    pub id: FunctionId,
    pub name: String,
    pub binding_syntax: Option<SyntaxId>,
    pub pattern: Pattern,
    pub return_annotation: Option<Type>,
    pub binding_annotation: Option<Type>,
    pub type_parameters: Vec<TypeParameterPattern>,
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
    nominal_patterns: HashMap<SyntaxId, TypeId>,
    type_parameters: HashMap<SyntaxId, TypeParameterId>,
    type_declarations: HashMap<TypeId, TypeDeclaration>,
    type_names: HashMap<TypeId, String>,
    syntax_modules: HashMap<SyntaxId, ModuleId>,
    lowered_infix: HashMap<SyntaxId, Expression>,
    builtin_types: HashMap<TypeId, BuiltinType>,
    intrinsic_functions: HashMap<SymbolId, IntrinsicFunction>,
    macro_calls: HashMap<SyntaxId, PrimitiveMacro>,
    constructors: HashMap<SymbolId, TypeId>,
    type_modules: HashMap<TypeId, ModuleId>,
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

    pub fn type_for_pattern(&self, syntax_id: SyntaxId) -> Option<TypeId> {
        self.nominal_patterns.get(&syntax_id).copied()
    }

    pub fn representation_visible_from(&self, id: TypeId, module: ModuleId) -> bool {
        self.type_modules.get(&id).copied() == Some(module)
            || self.type_declarations[&id].representation_visibility == Visibility::Public
    }

    pub fn type_parameter_for(&self, syntax_id: SyntaxId) -> Option<TypeParameterId> {
        self.type_parameters.get(&syntax_id).copied()
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

    pub fn lowered_infix(&self, syntax_id: SyntaxId) -> Option<&Expression> {
        self.lowered_infix.get(&syntax_id)
    }

    pub fn builtin_type(&self, id: TypeId) -> Option<BuiltinType> {
        self.builtin_types.get(&id).copied()
    }

    pub fn intrinsic_function(&self, symbol: SymbolId) -> Option<IntrinsicFunction> {
        self.intrinsic_functions.get(&symbol).copied()
    }

    pub fn intrinsic_functions(&self) -> &HashMap<SymbolId, IntrinsicFunction> {
        &self.intrinsic_functions
    }

    pub fn primitive_macro_for(&self, syntax: SyntaxId) -> Option<PrimitiveMacro> {
        self.macro_calls.get(&syntax).copied()
    }

    pub fn constructor_type(&self, symbol: SymbolId) -> Option<TypeId> {
        self.constructors.get(&symbol).copied()
    }

    pub fn constructors(&self) -> &HashMap<SymbolId, TypeId> {
        &self.constructors
    }
}

#[derive(Default)]
struct Interface {
    values: HashMap<String, SymbolId>,
    fixities: HashMap<String, Fixity>,
    types: HashMap<String, TypeId>,
    macros: HashMap<String, MacroId>,
}

#[derive(Default)]
pub struct NameResolver {
    scopes: Vec<HashMap<String, SymbolId>>,
    namespaces: HashMap<String, ModuleId>,
    imported_types: HashMap<String, TypeId>,
    imported_fixities: HashMap<String, Fixity>,
    imported_macros: HashMap<String, MacroId>,
    type_parameter_scopes: Vec<HashMap<String, TypeParameterId>>,
    prelude_values: HashMap<String, SymbolId>,
    prelude_types: HashMap<String, TypeId>,
    prelude_fixities: HashMap<String, Fixity>,
    prelude_macros: HashMap<String, MacroId>,
    symbols: HashMap<SyntaxId, SymbolId>,
    function_expressions: HashMap<SyntaxId, FunctionId>,
    symbol_owners: HashMap<SymbolId, Option<FunctionId>>,
    function_parents: HashMap<FunctionId, Option<FunctionId>>,
    function_captures: HashMap<FunctionId, Vec<SymbolId>>,
    function_stack: Vec<FunctionId>,
    declared_fixities: Vec<HashMap<String, Fixity>>,
    lowered_infix: HashMap<SyntaxId, Expression>,
    builtin_types: HashMap<TypeId, BuiltinType>,
    intrinsic_functions: HashMap<SymbolId, IntrinsicFunction>,
    primitive_macros: HashMap<MacroId, PrimitiveMacro>,
    macro_calls: HashMap<SyntaxId, PrimitiveMacro>,
    constructors: HashMap<SymbolId, TypeId>,
    nominal_patterns: HashMap<SyntaxId, TypeId>,
    type_modules: HashMap<TypeId, ModuleId>,
    type_constructor_symbols: HashMap<TypeId, SymbolId>,
    functions: Vec<ResolvedFunction>,
    named_types: HashMap<SyntaxId, TypeId>,
    type_parameters: HashMap<SyntaxId, TypeParameterId>,
    type_declarations: HashMap<TypeId, TypeDeclaration>,
    type_names: HashMap<TypeId, String>,
    syntax_modules: HashMap<SyntaxId, ModuleId>,
    interfaces: Vec<Interface>,
    declared_symbols: HashMap<SyntaxId, SymbolId>,
    binding_type_parameters: HashMap<SyntaxId, Vec<TypeParameterPattern>>,
    declared_types: Vec<HashMap<String, TypeId>>,
    declared_macros: Vec<HashMap<String, MacroId>>,
    diagnostics: Vec<Diagnostic>,
    next_symbol_id: usize,
    next_function_id: usize,
    next_macro_id: usize,
    next_type_parameter_id: usize,
    next_syntax_id: usize,
    current_module: ModuleId,
    multiple_modules: bool,
    standard_library_core: Option<ModuleId>,
    standard_library_cinterop: Option<ModuleId>,
}

impl NameResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn resolve(self, module: &Module) -> Result<ResolvedModule, Vec<Diagnostic>> {
        self.resolve_program(Program::single(module.clone()))
    }

    pub fn resolve_program(mut self, program: Program) -> Result<ResolvedModule, Vec<Diagnostic>> {
        self.standard_library_core = program.standard_library_core();
        self.standard_library_cinterop = program.standard_library_cinterop();
        self.multiple_modules = program
            .modules()
            .iter()
            .filter(|module| {
                Some(module.id) != self.standard_library_core
                    && Some(module.id) != self.standard_library_cinterop
            })
            .count()
            > 1;
        self.next_syntax_id = program
            .modules()
            .iter()
            .map(|module| module.syntax.syntax.id.0)
            .max()
            .unwrap_or(0)
            + 1;
        self.collect_interfaces(&program);
        self.collect_standard_library_contract(&program);
        for source_module in program.modules() {
            self.current_module = source_module.id;
            self.scopes.clear();
            self.namespaces.clear();
            self.imported_types.clear();
            self.imported_fixities.clear();
            self.imported_macros.clear();
            self.prelude_values.clear();
            self.prelude_types.clear();
            self.prelude_fixities.clear();
            self.prelude_macros.clear();
            self.push_scope();
            self.install_prelude(&program, source_module.id);
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
            nominal_patterns: self.nominal_patterns,
            type_parameters: self.type_parameters,
            type_declarations: self.type_declarations,
            type_names: self.type_names,
            syntax_modules: self.syntax_modules,
            lowered_infix: self.lowered_infix,
            builtin_types: self.builtin_types,
            intrinsic_functions: self.intrinsic_functions,
            macro_calls: self.macro_calls,
            constructors: self.constructors,
            type_modules: self.type_modules,
        })
    }

    fn collect_standard_library_contract(&mut self, program: &Program) {
        let Some(core) = program.standard_library_core() else {
            return;
        };
        self.register_builtin_type(core, "std.core", "I32", BuiltinType::I32);
        self.register_builtin_type(core, "std.core", "Bool", BuiltinType::Bool);
        self.register_builtin_type(core, "std.core", "String", BuiltinType::String);

        if let Some(cinterop) = program.standard_library_cinterop() {
            self.register_builtin_type(cinterop, "std.cinterop", "CChar", BuiltinType::CChar);
            self.register_builtin_type(cinterop, "std.cinterop", "CString", BuiltinType::CString);
            self.register_builtin_type(cinterop, "std.cinterop", "CPointer", BuiltinType::CPointer);
            self.register_primitive_macro(cinterop, "c_string", PrimitiveMacro::CString);
        }

        let expected = [
            ("__i32_add", IntrinsicFunction::I32Add),
            ("__i32_subtract", IntrinsicFunction::I32Subtract),
            ("__i32_multiply", IntrinsicFunction::I32Multiply),
            ("__i32_divide", IntrinsicFunction::I32Divide),
            (
                "__string_from_c_string",
                IntrinsicFunction::StringFromCString,
            ),
            ("__string_to_c_string", IntrinsicFunction::StringToCString),
        ];
        let mut found = HashMap::new();
        for source_module in [Some(core), program.standard_library_cinterop()]
            .into_iter()
            .flatten()
        {
            for item in &program.module(source_module).syntax.items {
                let Item::ExternBlock(block) = item else {
                    continue;
                };
                if block.abi != "\"staple-intrinsic\"" {
                    continue;
                }
                for binding in &block.bindings {
                    if let Some((_, intrinsic)) =
                        expected.iter().find(|(name, _)| *name == binding.name)
                        && let Some(symbol) = self.declared_symbols.get(&binding.syntax.id).copied()
                    {
                        found.insert(binding.name.clone(), symbol);
                        self.intrinsic_functions.insert(symbol, *intrinsic);
                    } else {
                        self.diagnostics.push(Diagnostic::new(
                            binding.syntax.span.clone(),
                            format!("unknown Staple intrinsic `{}`", binding.name),
                        ));
                    }
                }
            }
        }
        for (name, _) in expected {
            if !found.contains_key(name) {
                self.diagnostics.push(Diagnostic::new(
                    Span::Compiler,
                    format!("standard library `std.core` does not declare intrinsic `{name}`"),
                ));
            }
        }
        for source_module in program.modules() {
            if source_module.id == core
                || Some(source_module.id) == program.standard_library_cinterop()
            {
                continue;
            }
            for item in &source_module.syntax.items {
                if let Item::ExternBlock(block) = item
                    && block.abi == "\"staple-intrinsic\""
                {
                    self.diagnostics.push(Diagnostic::new(
                        block.syntax.span.clone(),
                        "the `staple-intrinsic` ABI is reserved for the standard library",
                    ));
                }
            }
        }
    }

    fn register_primitive_macro(
        &mut self,
        module: ModuleId,
        name: &str,
        primitive: PrimitiveMacro,
    ) {
        let Some(id) = self.declared_macros[module.0].get(name).copied() else {
            self.diagnostics.push(Diagnostic::new(
                Span::Compiler,
                format!("standard library `std.cinterop` does not declare macro `{name}`"),
            ));
            return;
        };
        if !self.interfaces[module.0].macros.contains_key(name) {
            self.diagnostics.push(Diagnostic::new(
                Span::Compiler,
                format!("standard library macro `{name}` must be public"),
            ));
        }
        self.primitive_macros.insert(id, primitive);
    }

    fn register_builtin_type(
        &mut self,
        module: ModuleId,
        module_name: &str,
        name: &str,
        builtin: BuiltinType,
    ) {
        let Some(id) = self.declared_types[module.0].get(name).copied() else {
            self.diagnostics.push(Diagnostic::new(
                Span::Compiler,
                format!("standard library `{module_name}` does not declare `{name}`"),
            ));
            return;
        };
        let declaration = &self.type_declarations[&id];
        if declaration.visibility != Visibility::Public {
            self.diagnostics.push(Diagnostic::new(
                declaration.syntax.span.clone(),
                format!("standard library type `{name}` must be public"),
            ));
        }
        if declaration.kind != crate::TypeDeclarationKind::Opaque {
            self.diagnostics.push(Diagnostic::new(
                declaration.syntax.span.clone(),
                format!("standard library type `{name}` must be opaque"),
            ));
        }
        if builtin == BuiltinType::CPointer && declaration.type_parameters.len() != 1 {
            self.diagnostics.push(Diagnostic::new(
                declaration.syntax.span.clone(),
                "standard library type `CPointer` must accept one compile-time argument",
            ));
        }
        self.type_names.insert(id, name.to_owned());
        self.builtin_types.insert(id, builtin);
    }

    fn collect_interfaces(&mut self, program: &Program) {
        self.interfaces = (0..program.modules().len())
            .map(|_| Interface::default())
            .collect();
        self.declared_types = (0..program.modules().len())
            .map(|_| HashMap::new())
            .collect();
        self.declared_macros = (0..program.modules().len())
            .map(|_| HashMap::new())
            .collect();
        self.declared_fixities = (0..program.modules().len())
            .map(|_| HashMap::new())
            .collect();
        for source_module in program.modules() {
            for item in &source_module.syntax.items {
                match item {
                    Item::ExternBlock(block) => {
                        for binding in &block.bindings {
                            let symbol = self.allocate_symbol(binding);
                            let fixity = binding.fixity.unwrap_or_default();
                            self.declared_fixities[source_module.id.0]
                                .insert(binding.name.clone(), fixity);
                            if block.visibility == Visibility::Public {
                                self.insert_public_value(
                                    source_module.id,
                                    &binding.name,
                                    symbol,
                                    binding.syntax.span.clone(),
                                );
                                self.interfaces[source_module.id.0]
                                    .fixities
                                    .insert(binding.name.clone(), fixity);
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
                        self.type_modules.insert(id, source_module.id);
                        if declaration.kind == crate::TypeDeclarationKind::Distinct
                            && declaration.underlying.is_some()
                        {
                            let symbol = SymbolId(self.next_symbol_id);
                            self.next_symbol_id += 1;
                            self.symbol_owners.insert(symbol, None);
                            self.type_constructor_symbols.insert(id, symbol);
                            self.constructors.insert(symbol, id);
                            if declaration.representation_visibility == Visibility::Public {
                                self.insert_public_value(
                                    source_module.id,
                                    &declaration.name,
                                    symbol,
                                    declaration.syntax.span.clone(),
                                );
                            }
                        }
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
                    Item::MacroDeclaration(declaration) => {
                        let id = MacroId(self.next_macro_id);
                        self.next_macro_id += 1;
                        if self.declared_macros[source_module.id.0]
                            .insert(declaration.name.clone(), id)
                            .is_some()
                        {
                            self.diagnostics.push(Diagnostic::new(
                                declaration.syntax.span.clone(),
                                format!("duplicate macro definition of `{}`", declaration.name),
                            ));
                        }
                        if declaration.visibility == Visibility::Public {
                            self.interfaces[source_module.id.0]
                                .macros
                                .insert(declaration.name.clone(), id);
                        }
                    }
                    Item::Statement(statement) => match statement.as_ref() {
                        Statement::Binding(binding) => {
                            let symbol = self.allocate_symbol(binding);
                            let fixity = binding.fixity.unwrap_or_default();
                            self.declared_fixities[source_module.id.0]
                                .insert(binding.name.clone(), fixity);
                            if binding.visibility == Visibility::Public {
                                self.insert_public_value(
                                    source_module.id,
                                    &binding.name,
                                    symbol,
                                    binding.syntax.span.clone(),
                                );
                                self.interfaces[source_module.id.0]
                                    .fixities
                                    .insert(binding.name.clone(), fixity);
                            }
                        }
                        Statement::PatternBinding(binding) => {
                            self.allocate_pattern_symbols(&binding.pattern);
                        }
                        Statement::Return(_) | Statement::Expression(_) => {}
                    },
                    Item::UseDeclaration(_) => {}
                }
            }
        }
    }

    fn allocate_symbol(&mut self, binding: &Binding) -> SymbolId {
        let symbol = SymbolId(self.next_symbol_id);
        self.next_symbol_id += 1;
        self.declared_symbols.insert(binding.syntax.id, symbol);
        self.binding_type_parameters
            .insert(binding.syntax.id, binding.type_parameters.clone());
        self.symbols.insert(binding.syntax.id, symbol);
        self.symbol_owners.insert(symbol, None);
        symbol
    }

    fn allocate_pattern_symbols(&mut self, pattern: &Pattern) {
        match pattern {
            Pattern::Binding(binding) => {
                let symbol = SymbolId(self.next_symbol_id);
                self.next_symbol_id += 1;
                self.declared_symbols.insert(binding.syntax.id, symbol);
                self.symbols.insert(binding.syntax.id, symbol);
                self.symbol_owners.insert(symbol, None);
            }
            Pattern::Product(product) => {
                for element in &product.elements {
                    self.allocate_pattern_symbols(element);
                }
            }
            Pattern::Nominal(pattern) => self.allocate_pattern_symbols(&pattern.argument),
        }
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
                        if let Some(fixity) =
                            self.interfaces[imported.0].fixities.get(&name).copied()
                        {
                            self.imported_fixities.insert(name.clone(), fixity);
                        }
                        self.insert_imported_value(name, symbol, declaration.syntax.span.clone());
                    }
                    for (name, ty) in self.interfaces[imported.0].types.clone() {
                        self.insert_imported_type(name, ty, declaration.syntax.span.clone());
                    }
                    for (name, macro_id) in self.interfaces[imported.0].macros.clone() {
                        self.insert_imported_macro(name, macro_id, declaration.syntax.span.clone());
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

    fn install_prelude(&mut self, program: &Program, module: ModuleId) {
        let Some(core) = program.standard_library_core() else {
            return;
        };
        if module == core {
            return;
        }
        self.prelude_values = self.interfaces[core.0].values.clone();
        self.prelude_types = self.interfaces[core.0].types.clone();
        self.prelude_fixities = self.interfaces[core.0].fixities.clone();
        self.prelude_macros = self.interfaces[core.0].macros.clone();
    }

    fn install_selected(&mut self, module: ModuleId, item: &str, local: &str, span: Span) {
        let mut found = false;
        if let Some(symbol) = self.interfaces[module.0].values.get(item).copied() {
            found = true;
            if let Some(fixity) = self.interfaces[module.0].fixities.get(item).copied() {
                self.imported_fixities.insert(local.to_owned(), fixity);
            }
            self.insert_imported_value(local.to_owned(), symbol, span.clone());
        }
        if let Some(ty) = self.interfaces[module.0].types.get(item).copied() {
            found = true;
            self.insert_imported_type(local.to_owned(), ty, span.clone());
        }
        if let Some(macro_id) = self.interfaces[module.0].macros.get(item).copied() {
            found = true;
            self.insert_imported_macro(local.to_owned(), macro_id, span.clone());
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

    fn insert_imported_macro(&mut self, name: String, id: MacroId, span: Span) {
        if self.current_scope().contains_key(&name)
            || self.namespaces.contains_key(&name)
            || self.imported_macros.insert(name.clone(), id).is_some()
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
                Item::UseDeclaration(_) | Item::TypeDeclaration(_) | Item::MacroDeclaration(_) => {}
            }
        }
        for item in items {
            let Item::TypeDeclaration(declaration) = item else {
                continue;
            };
            let Some(id) = self.declared_types[self.current_module.0]
                .get(&declaration.name)
                .copied()
            else {
                continue;
            };
            let Some(symbol) = self.type_constructor_symbols.get(&id).copied() else {
                continue;
            };
            if self.current_scope().contains_key(&declaration.name) {
                self.diagnostics.push(Diagnostic::new(
                    declaration.syntax.span.clone(),
                    format!("duplicate definition of `{}`", declaration.name),
                ));
            } else {
                self.current_scope_mut()
                    .insert(declaration.name.clone(), symbol);
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
            Item::TypeDeclaration(declaration) => {
                self.push_type_parameter_scope();
                for parameter in &declaration.type_parameters {
                    self.declare_type_parameter_pattern(parameter);
                }
                if let Some(underlying) = &declaration.underlying {
                    self.resolve_type(underlying);
                    if declaration.representation_visibility == Visibility::Public {
                        self.validate_public_representation(underlying);
                    }
                }
                self.pop_type_parameter_scope();
            }
            Item::MacroDeclaration(declaration) => {
                if !self
                    .primitive_macros
                    .contains_key(&self.declared_macros[self.current_module.0][&declaration.name])
                {
                    self.diagnostics.push(Diagnostic::new(
                        declaration.syntax.span.clone(),
                        "user-defined macros are not supported yet",
                    ));
                }
            }
            Item::Statement(statement) => self.resolve_statement(statement),
        }
    }

    fn resolve_statement(&mut self, statement: &Statement) {
        match statement {
            Statement::Binding(binding) => self.resolve_binding(binding),
            Statement::PatternBinding(binding) => {
                self.resolve_pattern_types(&binding.pattern);
                self.resolve_expression(&binding.value, None, None);
                self.declare_pattern(&binding.pattern);
            }
            Statement::Return(statement) => {
                if self.function_stack.is_empty() {
                    self.diagnostics.push(Diagnostic::new(
                        statement.syntax.span.clone(),
                        "`return` is only allowed inside a function",
                    ));
                }
                self.resolve_expression(&statement.value, None, None);
            }
            Statement::Expression(expression) => self.resolve_expression(expression, None, None),
        }
    }

    fn resolve_binding(&mut self, binding: &Binding) {
        self.binding_type_parameters
            .entry(binding.syntax.id)
            .or_insert_with(|| binding.type_parameters.clone());
        self.push_type_parameter_scope();
        for parameter in &binding.type_parameters {
            self.declare_type_parameter_pattern(parameter);
        }
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
        self.pop_type_parameter_scope();
    }

    fn push_type_parameter_scope(&mut self) {
        self.type_parameter_scopes.push(HashMap::new());
    }

    fn pop_type_parameter_scope(&mut self) {
        self.type_parameter_scopes.pop();
    }

    fn lookup_type_parameter(&self, name: &str) -> Option<TypeParameterId> {
        self.type_parameter_scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
    }

    fn declare_type_parameter_pattern(&mut self, pattern: &TypeParameterPattern) {
        match pattern {
            TypeParameterPattern::Binding(binding) => {
                let id = TypeParameterId(self.next_type_parameter_id);
                self.next_type_parameter_id += 1;
                let scope = self
                    .type_parameter_scopes
                    .last_mut()
                    .expect("type parameter scope");
                if scope.insert(binding.name.clone(), id).is_some() {
                    self.diagnostics.push(Diagnostic::new(
                        binding.syntax.span.clone(),
                        format!("duplicate compile-time parameter `{}`", binding.name),
                    ));
                }
                self.type_parameters.insert(binding.syntax.id, id);
            }
            TypeParameterPattern::Product(product) => {
                for element in &product.elements {
                    self.declare_type_parameter_pattern(element);
                }
            }
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
                self.resolve_pattern_types(&function.pattern);
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
                let base_name = mangle_function_name(&base_name);
                let name = if self.multiple_modules
                    || base_name == "main"
                    || Some(self.current_module) == self.standard_library_core
                    || Some(self.current_module) == self.standard_library_cinterop
                {
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
                    type_parameters: suggested_function
                        .and_then(|(_, syntax)| self.binding_type_parameters.get(&syntax).cloned())
                        .unwrap_or_default(),
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
                if let Some(primitive) = self.resolve_primitive_macro(&call.callee) {
                    self.macro_calls.insert(call.syntax.id, primitive);
                    self.resolve_expression(&call.argument, None, None);
                    return;
                }
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
                    } else if self.interfaces[module.0].macros.contains_key(item) {
                        self.diagnostics.push(Diagnostic::new(
                            access.syntax.span.clone(),
                            format!("macro `{item}` must be invoked"),
                        ));
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
            Expression::Infix(infix) => {
                let lowered = self.lower_infix(infix);
                self.resolve_expression(&lowered, expected_type, suggested_function);
                self.lowered_infix.insert(infix.syntax.id, lowered);
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
                None if self.lookup_macro(&name.name).is_some() => {
                    self.diagnostics.push(Diagnostic::new(
                        name.syntax.span.clone(),
                        format!("macro `{}` must be invoked", name.name),
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
                if named.namespace.is_none()
                    && let Some(id) = self.lookup_type_parameter(&named.name)
                {
                    self.type_parameters.insert(named.syntax.id, id);
                    return;
                }
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
                        .or_else(|| self.prelude_types.get(&named.name).copied())
                };
                if let Some(id) = resolved {
                    self.named_types.insert(named.syntax.id, id);
                } else if named.name != "int" {
                    self.diagnostics.push(Diagnostic::new(
                        named.syntax.span.clone(),
                        format!("unknown type `{}`", named.name),
                    ));
                }
            }
            Type::Product(product) => {
                for element in &product.elements {
                    self.resolve_type(&element.ty);
                }
            }
            Type::Function(function) => {
                self.resolve_type(&function.parameter);
                self.resolve_type(&function.result);
            }
            Type::Application(application) => {
                self.resolve_type(&application.callee);
                self.resolve_type(&application.argument);
            }
            Type::Inferred(_) => {}
        }
    }

    fn resolve_pattern_types(&mut self, pattern: &Pattern) {
        match pattern {
            Pattern::Binding(binding) => self.resolve_type(&binding.ty),
            Pattern::Product(product) => {
                for element in &product.elements {
                    self.resolve_pattern_types(element);
                }
            }
            Pattern::Nominal(pattern) => {
                self.resolve_type(&Type::Named(crate::NamedType {
                    syntax: pattern.syntax.clone(),
                    namespace: pattern.namespace.clone(),
                    name: pattern.name.clone(),
                }));
                self.resolve_pattern_types(&pattern.argument);
            }
        }
    }

    fn validate_public_representation(&mut self, ty: &Type) {
        match ty {
            Type::Named(named) => {
                if self.type_parameters.contains_key(&named.syntax.id) {
                    return;
                }
                if let Some(id) = self.named_types.get(&named.syntax.id).copied()
                    && self.type_declarations[&id].visibility != Visibility::Public
                {
                    self.diagnostics.push(Diagnostic::new(
                        named.syntax.span.clone(),
                        format!(
                            "public representation references private type `{}`",
                            named.name
                        ),
                    ));
                }
            }
            Type::Product(product) => {
                for element in &product.elements {
                    self.validate_public_representation(&element.ty);
                }
            }
            Type::Function(function) => {
                self.validate_public_representation(&function.parameter);
                self.validate_public_representation(&function.result);
            }
            Type::Application(application) => {
                self.validate_public_representation(&application.callee);
                self.validate_public_representation(&application.argument);
            }
            Type::Inferred(_) => {}
        }
    }

    fn lower_infix(&mut self, infix: &InfixExpression) -> Expression {
        let mut values = vec![infix.operands[0].clone()];
        let mut operators: Vec<(InfixOperator, Fixity)> = Vec::new();
        for (index, operator) in infix.operators.iter().cloned().enumerate() {
            let fixity = self.operator_fixity(&operator);
            while let Some((top_operator, top_fixity)) = operators.last() {
                let reduce = if top_fixity.precedence > fixity.precedence {
                    true
                } else if top_fixity.precedence < fixity.precedence {
                    false
                } else if top_fixity.associativity == fixity.associativity
                    && top_fixity.associativity != Associativity::None
                {
                    top_fixity.associativity == Associativity::Left
                } else {
                    self.diagnostics.push(Diagnostic::new(
                        operator.syntax.span.clone(),
                        format!(
                            "operators `{}` and `{}` have incompatible associativity at precedence {}",
                            top_operator.name, operator.name, fixity.precedence
                        ),
                    ));
                    true
                };
                if !reduce {
                    break;
                }
                let (operator, _) = operators.pop().expect("peeked operator");
                self.reduce_infix(&mut values, operator, infix.syntax.span.clone());
            }
            operators.push((operator, fixity));
            values.push(infix.operands[index + 1].clone());
        }
        while let Some((operator, _)) = operators.pop() {
            self.reduce_infix(&mut values, operator, infix.syntax.span.clone());
        }
        values.pop().expect("infix expression result")
    }

    fn reduce_infix(&mut self, values: &mut Vec<Expression>, operator: InfixOperator, span: Span) {
        let right = values.pop().expect("right infix operand");
        let left = values.pop().expect("left infix operand");
        let operator = self.operator_value(operator);
        let first_call = Expression::Call(CallExpression {
            syntax: self.fresh_syntax(span.clone()),
            callee: Box::new(operator),
            argument: Box::new(left),
        });
        values.push(Expression::Call(CallExpression {
            syntax: self.fresh_syntax(span),
            callee: Box::new(first_call),
            argument: Box::new(right),
        }));
    }

    fn operator_fixity(&self, operator: &InfixOperator) -> Fixity {
        if let Some(namespace) = &operator.namespace {
            return self
                .namespaces
                .get(namespace)
                .and_then(|module| self.interfaces[module.0].fixities.get(&operator.name))
                .copied()
                .unwrap_or_default();
        }
        self.declared_fixities[self.current_module.0]
            .get(&operator.name)
            .or_else(|| self.imported_fixities.get(&operator.name))
            .or_else(|| self.prelude_fixities.get(&operator.name))
            .copied()
            .unwrap_or_default()
    }

    fn operator_value(&mut self, operator: InfixOperator) -> Expression {
        if let Some(namespace) = operator.namespace {
            let namespace_syntax = self.fresh_syntax(operator.syntax.span.clone());
            Expression::Access(AccessExpression {
                syntax: operator.syntax,
                value: Box::new(Expression::Name(NameExpression {
                    syntax: namespace_syntax,
                    name: namespace,
                })),
                accessor: Accessor::Name(operator.name),
            })
        } else {
            Expression::Name(NameExpression {
                syntax: operator.syntax,
                name: operator.name,
            })
        }
    }

    fn fresh_syntax(&mut self, span: Span) -> Syntax {
        let id = SyntaxId(self.next_syntax_id);
        self.next_syntax_id += 1;
        Syntax::synthetic(id, span)
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
            Pattern::Binding(binding) => {
                if let Some(symbol) = self.declared_symbols.get(&binding.syntax.id).copied() {
                    self.declare_symbol(
                        &binding.name,
                        binding.syntax.id,
                        binding.syntax.span.clone(),
                        symbol,
                    );
                } else {
                    self.declare_fresh_name(
                        &binding.name,
                        binding.syntax.id,
                        binding.syntax.span.clone(),
                    );
                }
            }
            Pattern::Product(product) => {
                for element in &product.elements {
                    self.declare_pattern(element);
                }
            }
            Pattern::Nominal(pattern) => {
                if let Some(id) = self.named_types.get(&pattern.syntax.id).copied() {
                    let declaration = &self.type_declarations[&id];
                    if declaration.kind != crate::TypeDeclarationKind::Distinct
                        || declaration.underlying.is_none()
                    {
                        self.diagnostics.push(Diagnostic::new(
                            pattern.syntax.span.clone(),
                            format!("`{}` is not a represented nominal type", pattern.name),
                        ));
                    } else if self.type_modules.get(&id).copied() != Some(self.current_module)
                        && declaration.representation_visibility != Visibility::Public
                    {
                        self.diagnostics.push(Diagnostic::new(
                            pattern.syntax.span.clone(),
                            format!("the representation of `{}` is private", pattern.name),
                        ));
                    } else {
                        self.nominal_patterns.insert(pattern.syntax.id, id);
                    }
                }
                self.declare_pattern(&pattern.argument);
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
            .or_else(|| self.prelude_values.get(name).copied())
    }

    fn lookup_macro(&self, name: &str) -> Option<MacroId> {
        if self.lookup(name).is_some() {
            return None;
        }
        self.declared_macros[self.current_module.0]
            .get(name)
            .copied()
            .or_else(|| self.imported_macros.get(name).copied())
            .or_else(|| self.prelude_macros.get(name).copied())
    }

    fn resolve_primitive_macro(&self, callee: &Expression) -> Option<PrimitiveMacro> {
        let id = match callee {
            Expression::Name(name) => self.lookup_macro(&name.name),
            Expression::Access(access) => {
                let Expression::Name(namespace) = access.value.as_ref() else {
                    return None;
                };
                let Accessor::Name(item) = &access.accessor else {
                    return None;
                };
                self.namespaces
                    .get(&namespace.name)
                    .and_then(|module| self.interfaces[module.0].macros.get(item))
                    .copied()
            }
            _ => None,
        }?;
        self.primitive_macros.get(&id).copied()
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

fn mangle_function_name(name: &str) -> String {
    let mut characters = name.chars();
    let ordinary = characters
        .next()
        .is_some_and(|first| first == '_' || first.is_alphabetic())
        && characters.all(|character| character == '_' || character.is_alphanumeric());
    if ordinary {
        name.to_owned()
    } else {
        let encoded = name
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        format!("operator.{encoded}")
    }
}
