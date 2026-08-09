use std::collections::{HashMap, HashSet};

use crate::{
    AccessExpression, Accessor, Associativity, Binding, BindingKind, BlockExpression,
    CallExpression, Diagnostic, Expression, Fixity, InfixExpression, InfixOperator, Item, Module,
    ModuleId, NameExpression, Pattern, PatternBindingKind, Program, Span, Statement, Syntax,
    SyntaxId, Type, TypeDeclaration, TypeParameterPattern, UseKind, Visibility,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SymbolId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FunctionId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TraitId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TraitMethodId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeParameterId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MacroId(pub usize);

#[derive(Debug, Clone)]
pub struct ResolvedTrait {
    pub id: TraitId,
    pub declaration: crate::TraitDeclaration,
    pub parameter: TypeParameterId,
    pub methods: Vec<TraitMethodId>,
}

#[derive(Debug, Clone)]
pub struct ResolvedTraitImplementation {
    pub syntax: SyntaxId,
    pub trait_id: TraitId,
    pub target: Type,
    pub methods: HashMap<TraitMethodId, FunctionId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltinType {
    Integer(IntegerType),
    String,
    Ref,
    CChar,
    CString,
    CPointer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntegerType {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    ISize,
    USize,
}

impl IntegerType {
    pub const ALL: [Self; 10] = [
        Self::I8,
        Self::I16,
        Self::I32,
        Self::I64,
        Self::U8,
        Self::U16,
        Self::U32,
        Self::U64,
        Self::ISize,
        Self::USize,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::I8 => "I8",
            Self::I16 => "I16",
            Self::I32 => "I32",
            Self::I64 => "I64",
            Self::U8 => "U8",
            Self::U16 => "U16",
            Self::U32 => "U32",
            Self::U64 => "U64",
            Self::ISize => "ISize",
            Self::USize => "USize",
        }
    }

    pub fn intrinsic_name(self) -> &'static str {
        match self {
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::ISize => "isize",
            Self::USize => "usize",
        }
    }

    pub fn is_signed(self) -> bool {
        matches!(
            self,
            Self::I8 | Self::I16 | Self::I32 | Self::I64 | Self::ISize
        )
    }

    pub fn fixed_width(self) -> Option<u32> {
        match self {
            Self::I8 | Self::U8 => Some(8),
            Self::I16 | Self::U16 => Some(16),
            Self::I32 | Self::U32 => Some(32),
            Self::I64 | Self::U64 => Some(64),
            Self::ISize | Self::USize => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntegerBinaryOperation {
    Add,
    Subtract,
    Multiply,
    Divide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntegerCompareOperation {
    Equal,
    NotEqual,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrimitiveMacro {
    CString,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntrinsicFunction {
    IntegerBinary {
        integer: IntegerType,
        operation: IntegerBinaryOperation,
    },
    IntegerCompare {
        integer: IntegerType,
        operation: IntegerCompareOperation,
    },
    StringFromCString,
    StringToCString,
    ErasedProductLength,
    Drop,
}

#[derive(Debug, Clone)]
pub struct ResolvedFunction {
    pub id: FunctionId,
    pub name: String,
    pub binding_syntax: Option<SyntaxId>,
    pub pattern: Pattern,
    pub result_annotation: Option<Type>,
    pub binding_annotation: Option<Type>,
    pub type_parameters: Vec<TypeParameterPattern>,
    pub trait_bounds: Vec<crate::TraitBound>,
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
    external_symbols: HashSet<SymbolId>,
    macro_calls: HashMap<SyntaxId, PrimitiveMacro>,
    constructors: HashMap<SymbolId, TypeId>,
    singleton_values: HashMap<SymbolId, TypeId>,
    type_modules: HashMap<TypeId, ModuleId>,
    traits: HashMap<TraitId, ResolvedTrait>,
    trait_methods: HashMap<TraitMethodId, crate::TraitMember>,
    trait_method_traits: HashMap<TraitMethodId, TraitId>,
    trait_references: HashMap<SyntaxId, TraitId>,
    trait_method_references: HashMap<SyntaxId, Vec<TraitMethodId>>,
    trait_implementations: Vec<ResolvedTraitImplementation>,
    checked_initialization_symbols: HashSet<SymbolId>,
    checked_initialization_reads: HashSet<SyntaxId>,
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
        self.type_declarations[&id].kind == crate::TypeDeclarationKind::Singleton
            || self.type_modules.get(&id).copied() == Some(module)
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

    pub fn is_external_symbol(&self, symbol: SymbolId) -> bool {
        self.external_symbols.contains(&symbol)
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

    pub fn singleton_type(&self, symbol: SymbolId) -> Option<TypeId> {
        self.singleton_values.get(&symbol).copied()
    }

    pub fn singleton_values(&self) -> &HashMap<SymbolId, TypeId> {
        &self.singleton_values
    }

    pub fn traits(&self) -> &HashMap<TraitId, ResolvedTrait> {
        &self.traits
    }

    pub fn trait_method(&self, id: TraitMethodId) -> Option<&crate::TraitMember> {
        self.trait_methods.get(&id)
    }

    pub fn trait_for_method(&self, id: TraitMethodId) -> Option<TraitId> {
        self.trait_method_traits.get(&id).copied()
    }

    pub fn trait_for(&self, syntax: SyntaxId) -> Option<TraitId> {
        self.trait_references.get(&syntax).copied()
    }

    pub fn trait_methods_for_expression(&self, syntax: SyntaxId) -> &[TraitMethodId] {
        self.trait_method_references
            .get(&syntax)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn trait_implementations(&self) -> &[ResolvedTraitImplementation] {
        &self.trait_implementations
    }

    pub fn requires_initialization_state(&self, symbol: SymbolId) -> bool {
        self.checked_initialization_symbols.contains(&symbol)
    }

    pub fn requires_initialization_check(&self, syntax: SyntaxId) -> bool {
        self.checked_initialization_reads.contains(&syntax)
    }
}

#[derive(Clone, Default)]
struct Interface {
    values: HashMap<String, SymbolId>,
    fixities: HashMap<String, Fixity>,
    types: HashMap<String, TypeId>,
    macros: HashMap<String, MacroId>,
    traits: HashMap<String, TraitId>,
}

fn extend_interface(exported: &mut Interface, imported: &Interface) -> bool {
    let mut changed = false;
    for name in imported.values.keys() {
        changed |= export_interface_item(exported, imported, name, name);
    }
    for name in imported.types.keys() {
        changed |= export_interface_item(exported, imported, name, name);
    }
    for name in imported.macros.keys() {
        changed |= export_interface_item(exported, imported, name, name);
    }
    for name in imported.traits.keys() {
        changed |= export_interface_item(exported, imported, name, name);
    }
    changed
}

fn export_interface_item(
    exported: &mut Interface,
    imported: &Interface,
    item: &str,
    alias: &str,
) -> bool {
    let mut changed = false;
    if let Some(symbol) = imported.values.get(item) {
        changed |= exported.values.insert(alias.to_owned(), *symbol).is_none();
        if let Some(fixity) = imported.fixities.get(item) {
            exported.fixities.insert(alias.to_owned(), *fixity);
        }
    }
    if let Some(ty) = imported.types.get(item) {
        changed |= exported.types.insert(alias.to_owned(), *ty).is_none();
    }
    if let Some(macro_id) = imported.macros.get(item) {
        changed |= exported
            .macros
            .insert(alias.to_owned(), *macro_id)
            .is_none();
    }
    if let Some(trait_id) = imported.traits.get(item) {
        changed |= exported
            .traits
            .insert(alias.to_owned(), *trait_id)
            .is_none();
    }
    changed
}

#[derive(Default)]
pub struct NameResolver {
    scopes: Vec<HashMap<String, SymbolId>>,
    namespaces: HashMap<String, ModuleId>,
    imported_types: HashMap<String, TypeId>,
    imported_fixities: HashMap<String, Fixity>,
    imported_macros: HashMap<String, MacroId>,
    imported_traits: HashMap<String, TraitId>,
    visible_trait_methods: HashMap<String, Vec<TraitMethodId>>,
    type_parameter_scopes: Vec<HashMap<String, TypeParameterId>>,
    prelude_values: HashMap<String, SymbolId>,
    prelude_types: HashMap<String, TypeId>,
    prelude_fixities: HashMap<String, Fixity>,
    prelude_macros: HashMap<String, MacroId>,
    prelude_traits: HashMap<String, TraitId>,
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
    singleton_values: HashMap<SymbolId, TypeId>,
    nominal_patterns: HashMap<SyntaxId, TypeId>,
    type_modules: HashMap<TypeId, ModuleId>,
    type_constructor_symbols: HashMap<TypeId, SymbolId>,
    functions: Vec<ResolvedFunction>,
    named_types: HashMap<SyntaxId, TypeId>,
    type_parameters: HashMap<SyntaxId, TypeParameterId>,
    type_declarations: HashMap<TypeId, TypeDeclaration>,
    type_names: HashMap<TypeId, String>,
    traits: HashMap<TraitId, ResolvedTrait>,
    trait_methods: HashMap<TraitMethodId, crate::TraitMember>,
    trait_method_traits: HashMap<TraitMethodId, TraitId>,
    trait_member_ids: HashMap<(TraitId, String), TraitMethodId>,
    trait_references: HashMap<SyntaxId, TraitId>,
    trait_method_references: HashMap<SyntaxId, Vec<TraitMethodId>>,
    trait_implementations: Vec<ResolvedTraitImplementation>,
    syntax_modules: HashMap<SyntaxId, ModuleId>,
    interfaces: Vec<Interface>,
    declared_symbols: HashMap<SyntaxId, SymbolId>,
    module_values: Vec<HashMap<String, SymbolId>>,
    definition_context_values: Vec<HashMap<String, SymbolId>>,
    definition_context_types: Vec<HashMap<String, TypeId>>,
    definition_context_namespaces: Vec<HashMap<String, ModuleId>>,
    definition_context_fixities: Vec<HashMap<String, Fixity>>,
    binding_type_parameters: HashMap<SyntaxId, Vec<TypeParameterPattern>>,
    binding_trait_bounds: HashMap<SyntaxId, Vec<crate::TraitBound>>,
    declared_types: Vec<HashMap<String, TypeId>>,
    declared_macros: Vec<HashMap<String, MacroId>>,
    declared_traits: Vec<HashMap<String, TraitId>>,
    diagnostics: Vec<Diagnostic>,
    next_symbol_id: usize,
    next_function_id: usize,
    next_macro_id: usize,
    next_trait_id: usize,
    next_trait_method_id: usize,
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
        let program = crate::macro_expand::expand_program(program)?;
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
        self.build_definition_context_values(&program);
        self.collect_standard_library_contract(&program);
        for source_module in program.modules() {
            self.current_module = source_module.id;
            self.scopes.clear();
            self.namespaces.clear();
            self.imported_types.clear();
            self.imported_fixities.clear();
            self.imported_macros.clear();
            self.imported_traits.clear();
            self.visible_trait_methods.clear();
            self.prelude_values.clear();
            self.prelude_types.clear();
            self.prelude_fixities.clear();
            self.prelude_macros.clear();
            self.prelude_traits.clear();
            self.push_scope();
            self.install_prelude(&program, source_module.id);
            self.install_imports(&program, source_module.id);
            self.install_local_traits(source_module.id);
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
        let external_symbols = program
            .modules()
            .iter()
            .flat_map(|module| module.syntax.items.iter())
            .filter_map(|item| match item {
                Item::ExternBlock(block) if block.abi != "\"staple-intrinsic\"" => Some(block),
                _ => None,
            })
            .flat_map(|block| block.bindings.iter())
            .filter_map(|binding| self.declared_symbols.get(&binding.syntax.id).copied())
            .collect();
        let mut resolved = ResolvedModule {
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
            external_symbols,
            macro_calls: self.macro_calls,
            constructors: self.constructors,
            singleton_values: self.singleton_values,
            type_modules: self.type_modules,
            traits: self.traits,
            trait_methods: self.trait_methods,
            trait_method_traits: self.trait_method_traits,
            trait_references: self.trait_references,
            trait_method_references: self.trait_method_references,
            trait_implementations: self.trait_implementations,
            checked_initialization_symbols: HashSet::new(),
            checked_initialization_reads: HashSet::new(),
        };
        let analysis = InitializationAnalyzer::new(&resolved).analyze();
        if !analysis.diagnostics.is_empty() {
            return Err(analysis.diagnostics);
        }
        resolved.checked_initialization_symbols = analysis.checked_symbols;
        resolved.checked_initialization_reads = analysis.checked_reads;
        Ok(resolved)
    }

    fn collect_standard_library_contract(&mut self, program: &Program) {
        let Some(core) = program.standard_library_core() else {
            return;
        };
        for integer in IntegerType::ALL {
            self.register_builtin_type(
                core,
                "std.core",
                integer.name(),
                BuiltinType::Integer(integer),
            );
        }
        self.register_builtin_type(core, "std.core", "String", BuiltinType::String);
        self.register_builtin_type(core, "std.core", "Ref", BuiltinType::Ref);

        if let Some(cinterop) = program.standard_library_cinterop() {
            self.register_builtin_type(cinterop, "std.cinterop", "CChar", BuiltinType::CChar);
            self.register_builtin_type(cinterop, "std.cinterop", "CString", BuiltinType::CString);
            self.register_builtin_type(cinterop, "std.cinterop", "CPointer", BuiltinType::CPointer);
            self.register_primitive_macro(cinterop, "c_string", PrimitiveMacro::CString);
        }

        let mut expected = Vec::new();
        for integer in IntegerType::ALL {
            for (suffix, operation) in [
                ("add", IntegerBinaryOperation::Add),
                ("subtract", IntegerBinaryOperation::Subtract),
                ("multiply", IntegerBinaryOperation::Multiply),
                ("divide", IntegerBinaryOperation::Divide),
            ] {
                expected.push((
                    format!("__{}_{}", integer.intrinsic_name(), suffix),
                    IntrinsicFunction::IntegerBinary { integer, operation },
                ));
            }
            for (suffix, operation) in [
                ("equal", IntegerCompareOperation::Equal),
                ("not_equal", IntegerCompareOperation::NotEqual),
                ("less_than", IntegerCompareOperation::LessThan),
                (
                    "less_than_or_equal",
                    IntegerCompareOperation::LessThanOrEqual,
                ),
                ("greater_than", IntegerCompareOperation::GreaterThan),
                (
                    "greater_than_or_equal",
                    IntegerCompareOperation::GreaterThanOrEqual,
                ),
            ] {
                expected.push((
                    format!("__{}_{}", integer.intrinsic_name(), suffix),
                    IntrinsicFunction::IntegerCompare { integer, operation },
                ));
            }
        }
        expected.push((
            "__string_from_c_string".to_owned(),
            IntrinsicFunction::StringFromCString,
        ));
        expected.push((
            "__string_to_c_string".to_owned(),
            IntrinsicFunction::StringToCString,
        ));
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
        for item in &program.module(core).syntax.items {
            let Item::Statement(statement) = item else {
                continue;
            };
            let Statement::Binding(binding) = statement.as_ref() else {
                continue;
            };
            if binding.name == "drop"
                && let Some(symbol) = self.declared_symbols.get(&binding.syntax.id).copied()
            {
                self.intrinsic_functions
                    .insert(symbol, IntrinsicFunction::Drop);
            }
            if binding.name == "length"
                && let Some(symbol) = self.declared_symbols.get(&binding.syntax.id).copied()
            {
                self.intrinsic_functions
                    .insert(symbol, IntrinsicFunction::ErasedProductLength);
            }
        }
        for (name, _) in expected {
            if !found.contains_key(&name) {
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
        self.collect_reexports(program);
    }

    fn collect_reexports(&mut self, program: &Program) {
        loop {
            let previous = self.interfaces.clone();
            let mut changed = false;
            for source_module in program.modules() {
                for item in &source_module.syntax.items {
                    let Item::UseDeclaration(declaration) = item else {
                        continue;
                    };
                    if declaration.visibility != Visibility::Public {
                        continue;
                    }
                    let Some(imported) = program.imported_module(declaration.syntax.id) else {
                        continue;
                    };
                    let imported = &previous[imported.0];
                    let exported = &mut self.interfaces[source_module.id.0];
                    match &declaration.kind {
                        UseKind::Namespace => {}
                        UseKind::Glob => {
                            changed |= extend_interface(exported, imported);
                        }
                        UseKind::Selected(names) => {
                            for name in names {
                                changed |= export_interface_item(exported, imported, name, name);
                            }
                        }
                        UseKind::Renamed { item, alias } => {
                            changed |= export_interface_item(exported, imported, item, alias);
                        }
                    }
                }
            }
            if !changed {
                break;
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
        let valid_kind = if builtin == BuiltinType::Ref {
            declaration.kind == crate::TypeDeclarationKind::Distinct
                && declaration.representation_visibility == Visibility::Public
        } else {
            declaration.kind == crate::TypeDeclarationKind::Opaque
        };
        if !valid_kind {
            self.diagnostics.push(Diagnostic::new(
                declaration.syntax.span.clone(),
                format!("standard library type `{name}` has an invalid representation"),
            ));
        }
        if matches!(builtin, BuiltinType::CPointer | BuiltinType::Ref)
            && declaration.type_parameters.len() != 1
        {
            self.diagnostics.push(Diagnostic::new(
                declaration.syntax.span.clone(),
                format!("standard library type `{name}` must accept one compile-time argument"),
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
        self.declared_traits = (0..program.modules().len())
            .map(|_| HashMap::new())
            .collect();
        self.declared_fixities = (0..program.modules().len())
            .map(|_| HashMap::new())
            .collect();
        self.module_values = (0..program.modules().len())
            .map(|_| HashMap::new())
            .collect();
        for source_module in program.modules() {
            for item in &source_module.syntax.items {
                match item {
                    Item::ExternBlock(block) => {
                        for binding in &block.bindings {
                            let symbol = self.allocate_symbol(binding);
                            self.module_values[source_module.id.0]
                                .insert(binding.name.clone(), symbol);
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
                        if (declaration.kind == crate::TypeDeclarationKind::Distinct
                            && declaration.underlying.is_some())
                            || declaration.kind == crate::TypeDeclarationKind::Singleton
                        {
                            let symbol = SymbolId(self.next_symbol_id);
                            self.next_symbol_id += 1;
                            self.symbol_owners.insert(symbol, None);
                            self.type_constructor_symbols.insert(id, symbol);
                            self.module_values[source_module.id.0]
                                .insert(declaration.name.clone(), symbol);
                            if declaration.kind == crate::TypeDeclarationKind::Singleton {
                                self.singleton_values.insert(symbol, id);
                            } else {
                                self.constructors.insert(symbol, id);
                            }
                            if declaration.representation_visibility == Visibility::Public
                                || (declaration.kind == crate::TypeDeclarationKind::Singleton
                                    && declaration.visibility == Visibility::Public)
                            {
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
                    Item::TraitDeclaration(declaration) => {
                        let id = TraitId(self.next_trait_id);
                        self.next_trait_id += 1;
                        if self.declared_traits[source_module.id.0]
                            .insert(declaration.name.clone(), id)
                            .is_some()
                        {
                            self.diagnostics.push(Diagnostic::new(
                                declaration.syntax.span.clone(),
                                format!("duplicate trait definition of `{}`", declaration.name),
                            ));
                        }
                        let parameter = TypeParameterId(self.next_type_parameter_id);
                        self.next_type_parameter_id += 1;
                        self.type_parameters
                            .insert(declaration.parameter.syntax.id, parameter);
                        let mut methods = Vec::new();
                        for member in &declaration.members {
                            let method = TraitMethodId(self.next_trait_method_id);
                            self.next_trait_method_id += 1;
                            if self
                                .trait_member_ids
                                .insert((id, member.name.clone()), method)
                                .is_some()
                            {
                                self.diagnostics.push(Diagnostic::new(
                                    member.syntax.span.clone(),
                                    format!("duplicate trait member `{}`", member.name),
                                ));
                            }
                            self.trait_methods.insert(method, member.clone());
                            self.trait_method_traits.insert(method, id);
                            methods.push(method);
                        }
                        self.traits.insert(
                            id,
                            ResolvedTrait {
                                id,
                                declaration: declaration.clone(),
                                parameter,
                                methods,
                            },
                        );
                        if declaration.visibility == Visibility::Public {
                            self.interfaces[source_module.id.0]
                                .traits
                                .insert(declaration.name.clone(), id);
                        }
                    }
                    Item::TraitImplementation(_) => {}
                    Item::Statement(statement) => match statement.as_ref() {
                        Statement::Binding(binding) => {
                            let symbol = self.allocate_symbol(binding);
                            self.module_values[source_module.id.0]
                                .insert(binding.name.clone(), symbol);
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

    fn build_definition_context_values(&mut self, program: &Program) {
        self.definition_context_values = self.module_values.clone();
        self.definition_context_types = self.declared_types.clone();
        self.definition_context_fixities = self.declared_fixities.clone();
        self.definition_context_namespaces = (0..program.modules().len())
            .map(|_| HashMap::new())
            .collect();
        if let Some(core) = program.standard_library_core() {
            for module in program.modules() {
                if module.id != core {
                    self.definition_context_values[module.id.0]
                        .extend(self.interfaces[core.0].values.clone());
                    self.definition_context_types[module.id.0]
                        .extend(self.interfaces[core.0].types.clone());
                    self.definition_context_fixities[module.id.0]
                        .extend(self.interfaces[core.0].fixities.clone());
                }
            }
        }
        for module in program.modules() {
            for item in &module.syntax.items {
                let Item::UseDeclaration(use_) = item else {
                    continue;
                };
                let Some(imported) = program.imported_module(use_.syntax.id) else {
                    continue;
                };
                match &use_.kind {
                    UseKind::Glob => {
                        self.definition_context_values[module.id.0]
                            .extend(self.interfaces[imported.0].values.clone());
                        self.definition_context_types[module.id.0]
                            .extend(self.interfaces[imported.0].types.clone());
                        self.definition_context_fixities[module.id.0]
                            .extend(self.interfaces[imported.0].fixities.clone());
                    }
                    UseKind::Selected(names) => {
                        for name in names {
                            if let Some(symbol) = self.interfaces[imported.0].values.get(name) {
                                self.definition_context_values[module.id.0]
                                    .insert(name.clone(), *symbol);
                            }
                            if let Some(ty) = self.interfaces[imported.0].types.get(name) {
                                self.definition_context_types[module.id.0]
                                    .insert(name.clone(), *ty);
                            }
                            if let Some(fixity) = self.interfaces[imported.0].fixities.get(name) {
                                self.definition_context_fixities[module.id.0]
                                    .insert(name.clone(), *fixity);
                            }
                        }
                    }
                    UseKind::Renamed { item, alias } => {
                        if let Some(symbol) = self.interfaces[imported.0].values.get(item) {
                            self.definition_context_values[module.id.0]
                                .insert(alias.clone(), *symbol);
                        }
                        if let Some(ty) = self.interfaces[imported.0].types.get(item) {
                            self.definition_context_types[module.id.0].insert(alias.clone(), *ty);
                        }
                        if let Some(fixity) = self.interfaces[imported.0].fixities.get(item) {
                            self.definition_context_fixities[module.id.0]
                                .insert(alias.clone(), *fixity);
                        }
                    }
                    UseKind::Namespace => {
                        if let Some(name) = use_.path.last() {
                            self.definition_context_namespaces[module.id.0]
                                .insert(name.clone(), imported);
                        }
                    }
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
        self.binding_trait_bounds
            .insert(binding.syntax.id, binding.trait_bounds.clone());
        self.symbols.insert(binding.syntax.id, symbol);
        self.symbol_owners.insert(symbol, None);
        symbol
    }

    fn allocate_pattern_symbols(&mut self, pattern: &Pattern) {
        match pattern {
            Pattern::Wildcard(_) => {}
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
                    for (name, trait_id) in self.interfaces[imported.0].traits.clone() {
                        self.insert_imported_trait(name, trait_id, declaration.syntax.span.clone());
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
        self.prelude_traits = self.interfaces[core.0].traits.clone();
        for trait_id in self.prelude_traits.values().copied().collect::<Vec<_>>() {
            self.add_visible_trait_methods(trait_id);
        }
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
        if let Some(trait_id) = self.interfaces[module.0].traits.get(item).copied() {
            found = true;
            self.insert_imported_trait(local.to_owned(), trait_id, span.clone());
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

    fn insert_imported_trait(&mut self, name: String, id: TraitId, span: Span) {
        if self.declared_traits[self.current_module.0].contains_key(&name)
            || self.imported_traits.insert(name.clone(), id).is_some()
        {
            self.duplicate_import(&name, span);
        } else {
            self.add_visible_trait_methods(id);
        }
    }

    fn install_local_traits(&mut self, module: ModuleId) {
        for trait_id in self.declared_traits[module.0]
            .values()
            .copied()
            .collect::<Vec<_>>()
        {
            self.add_visible_trait_methods(trait_id);
        }
    }

    fn add_visible_trait_methods(&mut self, trait_id: TraitId) {
        let methods = self.traits[&trait_id].methods.clone();
        for method in methods {
            let name = self.trait_methods[&method].name.clone();
            let candidates = self.visible_trait_methods.entry(name).or_default();
            if !candidates.contains(&method) {
                candidates.push(method);
            }
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
                Item::UseDeclaration(_)
                | Item::TypeDeclaration(_)
                | Item::MacroDeclaration(_)
                | Item::TraitDeclaration(_)
                | Item::TraitImplementation(_) => {}
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
                let _ = declaration;
            }
            Item::TraitDeclaration(declaration) => {
                let Some(id) = self.declared_traits[self.current_module.0]
                    .get(&declaration.name)
                    .copied()
                else {
                    return;
                };
                self.push_type_parameter_scope();
                self.type_parameter_scopes
                    .last_mut()
                    .expect("trait parameter scope")
                    .insert(
                        declaration.parameter.name.clone(),
                        self.traits[&id].parameter,
                    );
                for member in &declaration.members {
                    self.resolve_type(&member.annotation);
                    if declaration.visibility == Visibility::Public {
                        self.validate_public_representation(&member.annotation);
                    }
                }
                self.pop_type_parameter_scope();
            }
            Item::TraitImplementation(implementation) => {
                let trait_id = self.resolve_trait_name(&implementation.trait_name);
                self.resolve_type(&implementation.target);
                let mut methods = HashMap::new();
                for member in &implementation.members {
                    let method = trait_id.and_then(|trait_id| {
                        self.trait_member_ids
                            .get(&(trait_id, member.name.clone()))
                            .copied()
                    });
                    if trait_id.is_some() && method.is_none() {
                        self.diagnostics.push(Diagnostic::new(
                            member.syntax.span.clone(),
                            format!("trait has no member named `{}`", member.name),
                        ));
                    }
                    let function_name = trait_id
                        .map(|id| {
                            format!(
                                "impl.{}.{}.{}",
                                id.0, implementation.syntax.id.0, member.name
                            )
                        })
                        .unwrap_or_else(|| format!("impl.unknown.{}", member.name));
                    self.resolve_expression(
                        &member.value,
                        None,
                        Some((&function_name, member.syntax.id)),
                    );
                    if let Some(method) = method {
                        if let Some(function) = self
                            .function_expressions
                            .get(&member.value.syntax().id)
                            .copied()
                        {
                            if methods.insert(method, function).is_some() {
                                self.diagnostics.push(Diagnostic::new(
                                    member.syntax.span.clone(),
                                    format!("duplicate implementation member `{}`", member.name),
                                ));
                            }
                        } else {
                            self.diagnostics.push(Diagnostic::new(
                                member.value.syntax().span.clone(),
                                "trait implementation members must be function values",
                            ));
                        }
                    }
                }
                if let Some(trait_id) = trait_id {
                    self.trait_implementations
                        .push(ResolvedTraitImplementation {
                            syntax: implementation.syntax.id,
                            trait_id,
                            target: implementation.target.clone(),
                            methods,
                        });
                }
            }
            Item::Statement(statement) => self.resolve_statement(statement),
        }
    }

    fn resolve_statement(&mut self, statement: &Statement) {
        match statement {
            Statement::Binding(binding) => self.resolve_binding(binding),
            Statement::PatternBinding(binding) => {
                if binding.kind == PatternBindingKind::Propagating {
                    if self.function_stack.is_empty() {
                        self.diagnostics.push(Diagnostic::new(
                            binding.syntax.span.clone(),
                            "propagating bindings are only allowed inside a function",
                        ));
                    }
                    if !matches!(binding.pattern, Pattern::Nominal(_)) {
                        self.diagnostics.push(Diagnostic::new(
                            binding.pattern.syntax().span.clone(),
                            "a propagating binding requires a nominal pattern",
                        ));
                    }
                }
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
        self.binding_trait_bounds
            .entry(binding.syntax.id)
            .or_insert_with(|| binding.trait_bounds.clone());
        self.push_type_parameter_scope();
        for parameter in &binding.type_parameters {
            self.declare_type_parameter_pattern(parameter);
        }
        if let Some(annotation) = &binding.annotation {
            self.resolve_type(annotation);
        }
        for bound in &binding.trait_bounds {
            if let Some(trait_id) = self.resolve_trait_name(&bound.trait_name) {
                self.trait_references.insert(bound.syntax.id, trait_id);
            }
            self.resolve_type(&bound.argument);
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
                self.function_stack.push(function_id);
                self.push_scope();
                self.declare_pattern(&function.pattern);
                let expected_result = (|| {
                    let Type::Function(function_type) = expected_type? else {
                        return None;
                    };
                    Some(function_type.result.as_ref())
                })();
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
                let captures = self
                    .function_captures
                    .remove(&function_id)
                    .unwrap_or_default();
                self.functions.push(ResolvedFunction {
                    id: function_id,
                    name,
                    binding_syntax: suggested_function.map(|(_, syntax)| syntax),
                    pattern: function.pattern.clone(),
                    result_annotation: match function.body.as_ref() {
                        Expression::Satisfies(satisfies) => Some(satisfies.ty.clone()),
                        _ => None,
                    },
                    binding_annotation: expected_type.cloned(),
                    type_parameters: suggested_function
                        .and_then(|(_, syntax)| self.binding_type_parameters.get(&syntax).cloned())
                        .unwrap_or_default(),
                    trait_bounds: suggested_function
                        .and_then(|(_, syntax)| self.binding_trait_bounds.get(&syntax).cloned())
                        .unwrap_or_default(),
                    captures,
                    body: (*function.body).clone(),
                });
            }
            Expression::Satisfies(satisfies) => {
                self.resolve_type(&satisfies.ty);
                self.resolve_expression(&satisfies.value, Some(&satisfies.ty), suggested_function);
            }
            Expression::Match(match_) => {
                self.resolve_expression(&match_.subject, None, None);
                for arm in &match_.arms {
                    self.resolve_pattern_types(&arm.pattern);
                    self.push_scope();
                    self.declare_pattern(&arm.pattern);
                    self.resolve_expression(&arm.body, expected_type, None);
                    self.pop_scope();
                }
            }
            Expression::Block(block) => self.resolve_block(block),
            Expression::Product(product) => {
                for element in &product.elements {
                    let singleton_expected = (product.elements.len() == 1)
                        .then_some(expected_type)
                        .flatten();
                    let singleton_suggestion = (product.elements.len() == 1)
                        .then_some(suggested_function)
                        .flatten();
                    self.resolve_expression(
                        &element.value,
                        singleton_expected,
                        singleton_suggestion,
                    );
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
                if let Accessor::Name(member_name) = &access.accessor
                    && let Some(trait_id) = self.trait_id_from_expression(&access.value)
                {
                    if let Some(method) = self
                        .trait_member_ids
                        .get(&(trait_id, member_name.clone()))
                        .copied()
                    {
                        self.trait_method_references
                            .insert(access.syntax.id, vec![method]);
                    } else {
                        self.diagnostics.push(Diagnostic::new(
                            access.syntax.span.clone(),
                            format!("trait has no member named `{member_name}`"),
                        ));
                    }
                } else if let Expression::Name(namespace) = access.value.as_ref()
                    && let crate::Accessor::Name(item) = &access.accessor
                    && let Some(module) = namespace
                        .syntax
                        .definition_module()
                        .and_then(|context| {
                            self.definition_context_namespaces
                                .get(context)
                                .and_then(|namespaces| namespaces.get(&namespace.name))
                                .copied()
                        })
                        .or_else(|| self.namespaces.get(&namespace.name).copied())
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
            Expression::Index(index) => {
                self.resolve_expression(&index.value, None, None);
                self.resolve_expression(&index.index, None, None);
            }
            Expression::Infix(infix) => {
                let lowered = self.lower_infix(infix);
                self.resolve_expression(&lowered, expected_type, suggested_function);
                self.lowered_infix.insert(infix.syntax.id, lowered);
            }
            Expression::Name(name) => match (
                name.syntax
                    .definition_module()
                    .and_then(|module| {
                        self.definition_context_values
                            .get(module)
                            .and_then(|values| values.get(&name.name))
                            .copied()
                    })
                    .or_else(|| self.lookup(&name.name)),
                self.visible_trait_methods
                    .get(&name.name)
                    .cloned()
                    .unwrap_or_default(),
            ) {
                (Some(symbol), methods)
                    if !methods.is_empty()
                        && self.intrinsic_functions.get(&symbol)
                            != Some(&IntrinsicFunction::Drop) =>
                {
                    self.diagnostics.push(Diagnostic::new(
                        name.syntax.span.clone(),
                        format!("ambiguous name `{}`; qualify the trait method", name.name),
                    ))
                }
                (Some(symbol), _) => {
                    self.symbols.insert(name.syntax.id, symbol);
                    self.record_capture(symbol);
                }
                (None, methods) if !methods.is_empty() => {
                    self.trait_method_references.insert(name.syntax.id, methods);
                }
                (None, _) if self.namespaces.contains_key(&name.name) => {
                    self.diagnostics.push(Diagnostic::new(
                        name.syntax.span.clone(),
                        format!("module namespace `{}` is not a value", name.name),
                    ))
                }
                (None, _) if self.lookup_macro(&name.name).is_some() => {
                    self.diagnostics.push(Diagnostic::new(
                        name.syntax.span.clone(),
                        format!("macro `{}` must be invoked", name.name),
                    ))
                }
                (None, _) => self.diagnostics.push(Diagnostic::new(
                    name.syntax.span.clone(),
                    format!("unknown name `{}`", name.name),
                )),
            },
            Expression::Quote(quote) => self.diagnostics.push(Diagnostic::new(
                quote.syntax.span.clone(),
                "`quote` is only available during macro expansion",
            )),
            Expression::Splice(splice) => self.diagnostics.push(Diagnostic::new(
                splice.syntax.span.clone(),
                "splices are only available during macro expansion",
            )),
            Expression::String(_) | Expression::CString(_) | Expression::Integer(_) => {}
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
                    named
                        .syntax
                        .definition_module()
                        .and_then(|context| {
                            self.definition_context_namespaces
                                .get(context)
                                .and_then(|namespaces| namespaces.get(namespace))
                                .copied()
                        })
                        .or_else(|| self.namespaces.get(namespace).copied())
                        .and_then(|module| self.interfaces[module.0].types.get(&named.name))
                        .copied()
                } else {
                    named
                        .syntax
                        .definition_module()
                        .and_then(|module| {
                            self.definition_context_types
                                .get(module)
                                .and_then(|types| types.get(&named.name))
                                .copied()
                        })
                        .or_else(|| {
                            self.declared_types[self.current_module.0]
                                .get(&named.name)
                                .copied()
                        })
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
            Type::Sum(sum) => {
                for alternative in &sum.alternatives {
                    self.resolve_type(alternative);
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
            Type::Repeated(repeated) => self.resolve_type(&repeated.element),
            Type::Inferred(_) => {}
        }
    }

    fn resolve_trait_name(&mut self, name: &crate::NamedType) -> Option<TraitId> {
        let resolved = if let Some(namespace) = &name.namespace {
            self.namespaces
                .get(namespace)
                .and_then(|module| self.interfaces[module.0].traits.get(&name.name))
                .copied()
        } else {
            self.declared_traits[self.current_module.0]
                .get(&name.name)
                .copied()
                .or_else(|| self.imported_traits.get(&name.name).copied())
                .or_else(|| self.prelude_traits.get(&name.name).copied())
        };
        if let Some(id) = resolved {
            self.trait_references.insert(name.syntax.id, id);
        } else {
            self.diagnostics.push(Diagnostic::new(
                name.syntax.span.clone(),
                format!("unknown trait `{}`", name.name),
            ));
        }
        resolved
    }

    fn trait_id_from_expression(&self, expression: &Expression) -> Option<TraitId> {
        match expression {
            Expression::Name(name) => self.declared_traits[self.current_module.0]
                .get(&name.name)
                .copied()
                .or_else(|| self.imported_traits.get(&name.name).copied())
                .or_else(|| self.prelude_traits.get(&name.name).copied()),
            Expression::Access(access) => {
                let Expression::Name(namespace) = access.value.as_ref() else {
                    return None;
                };
                let Accessor::Name(name) = &access.accessor else {
                    return None;
                };
                self.namespaces
                    .get(&namespace.name)
                    .and_then(|module| self.interfaces[module.0].traits.get(name))
                    .copied()
            }
            _ => None,
        }
    }

    fn resolve_pattern_types(&mut self, pattern: &Pattern) {
        match pattern {
            Pattern::Wildcard(_) => {}
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
            Type::Sum(sum) => {
                for alternative in &sum.alternatives {
                    self.validate_public_representation(alternative);
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
            Type::Repeated(repeated) => self.validate_public_representation(&repeated.element),
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
                .definition_context_namespaces
                .get(
                    operator
                        .syntax
                        .definition_module()
                        .unwrap_or(self.current_module.0),
                )
                .and_then(|namespaces| namespaces.get(namespace))
                .copied()
                .or_else(|| self.namespaces.get(namespace).copied())
                .and_then(|module| self.interfaces[module.0].fixities.get(&operator.name))
                .copied()
                .unwrap_or_default();
        }
        operator
            .syntax
            .definition_module()
            .and_then(|module| self.definition_context_fixities[module].get(&operator.name))
            .or_else(|| self.declared_fixities[self.current_module.0].get(&operator.name))
            .or_else(|| self.imported_fixities.get(&operator.name))
            .or_else(|| self.prelude_fixities.get(&operator.name))
            .copied()
            .unwrap_or_default()
    }

    fn operator_value(&mut self, operator: InfixOperator) -> Expression {
        if let Some(namespace) = operator.namespace {
            let mut namespace_syntax = operator.syntax.clone();
            namespace_syntax.id = SyntaxId(self.next_syntax_id);
            self.next_syntax_id += 1;
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
            Pattern::Wildcard(_) => {}
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
                    let represented = (declaration.kind == crate::TypeDeclarationKind::Distinct
                        && declaration.underlying.is_some())
                        || declaration.kind == crate::TypeDeclarationKind::Singleton;
                    if !represented {
                        self.diagnostics.push(Diagnostic::new(
                            pattern.syntax.span.clone(),
                            format!("`{}` is not a represented nominal type", pattern.name),
                        ));
                    } else if declaration.kind != crate::TypeDeclarationKind::Singleton
                        && self.type_modules.get(&id).copied() != Some(self.current_module)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitializationState {
    Declared,
    Initializing,
    Initialized,
}

struct InitializationAnalysis {
    checked_symbols: HashSet<SymbolId>,
    checked_reads: HashSet<SyntaxId>,
    diagnostics: Vec<Diagnostic>,
}

struct InitializationAnalyzer<'a> {
    module: &'a ResolvedModule,
    checked_symbols: HashSet<SymbolId>,
    checked_reads: HashSet<SyntaxId>,
    diagnosed_reads: HashSet<SyntaxId>,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> InitializationAnalyzer<'a> {
    fn new(module: &'a ResolvedModule) -> Self {
        Self {
            module,
            checked_symbols: HashSet::new(),
            checked_reads: HashSet::new(),
            diagnosed_reads: HashSet::new(),
            diagnostics: Vec::new(),
        }
    }

    fn analyze(mut self) -> InitializationAnalysis {
        let mut globals = HashMap::new();
        for source_module in self.module.program().modules() {
            for item in &source_module.syntax.items {
                let Item::Statement(statement) = item else {
                    continue;
                };
                match statement.as_ref() {
                    Statement::Binding(binding) => {
                        if let Some(symbol) = self.module.symbol_for(binding.syntax.id) {
                            globals.insert(symbol, InitializationState::Declared);
                        }
                    }
                    Statement::PatternBinding(binding) => {
                        self.set_pattern_state(
                            &binding.pattern,
                            InitializationState::Declared,
                            &mut globals,
                        );
                    }
                    Statement::Return(_) | Statement::Expression(_) => {}
                }
            }
        }

        for module_id in self.module.program().initialization_order() {
            let items = self
                .module
                .program()
                .module(*module_id)
                .syntax
                .items
                .clone();
            for item in &items {
                if let Item::Statement(statement) = item {
                    self.statement(statement, &mut globals, &HashMap::new(), true);
                }
            }
        }

        InitializationAnalysis {
            checked_symbols: self.checked_symbols,
            checked_reads: self.checked_reads,
            diagnostics: self.diagnostics,
        }
    }

    fn statement(
        &mut self,
        statement: &Statement,
        local: &mut HashMap<SymbolId, InitializationState>,
        outer: &HashMap<SymbolId, InitializationState>,
        module_level: bool,
    ) {
        match statement {
            Statement::Binding(binding) => {
                let symbol = self.module.symbol_for(binding.syntax.id);
                if binding.kind == BindingKind::Def {
                    if let Some(symbol) = symbol
                        && binding.value.is_some()
                    {
                        local.insert(symbol, InitializationState::Initializing);
                    }
                    if let Some(value) = &binding.value {
                        self.expression(value, local, outer);
                        if let Some(symbol) = symbol {
                            local.insert(symbol, InitializationState::Initialized);
                        }
                    }
                } else {
                    if let Some(value) = &binding.value {
                        self.expression(value, local, outer);
                    }
                    if let Some(symbol) = symbol {
                        local.insert(
                            symbol,
                            if binding.value.is_some() {
                                InitializationState::Initialized
                            } else {
                                InitializationState::Declared
                            },
                        );
                    }
                }
            }
            Statement::PatternBinding(binding) => {
                if module_level {
                    self.set_pattern_state(
                        &binding.pattern,
                        InitializationState::Initializing,
                        local,
                    );
                }
                self.expression(&binding.value, local, outer);
                self.set_pattern_state(&binding.pattern, InitializationState::Initialized, local);
            }
            Statement::Return(statement) => self.expression(&statement.value, local, outer),
            Statement::Expression(expression) => self.expression(expression, local, outer),
        }
    }

    fn expression(
        &mut self,
        expression: &Expression,
        local: &mut HashMap<SymbolId, InitializationState>,
        outer: &HashMap<SymbolId, InitializationState>,
    ) {
        match expression {
            Expression::Function(function) => {
                let mut snapshot = outer.clone();
                snapshot.extend(local.iter().map(|(symbol, state)| (*symbol, *state)));
                let mut function_local = HashMap::new();
                self.set_pattern_state(
                    &function.pattern,
                    InitializationState::Initialized,
                    &mut function_local,
                );
                self.expression(&function.body, &mut function_local, &snapshot);
            }
            Expression::Satisfies(satisfies) => self.expression(&satisfies.value, local, outer),
            Expression::Match(match_) => {
                self.expression(&match_.subject, local, outer);
                for arm in &match_.arms {
                    let mut arm_local = local.clone();
                    self.set_pattern_state(
                        &arm.pattern,
                        InitializationState::Initialized,
                        &mut arm_local,
                    );
                    self.expression(&arm.body, &mut arm_local, outer);
                }
            }
            Expression::Block(block) => {
                let original = local.clone();
                for statement in &block.statements {
                    if let Statement::Binding(binding) = statement
                        && binding.kind == BindingKind::Def
                        && let Some(symbol) = self.module.symbol_for(binding.syntax.id)
                    {
                        local.insert(symbol, InitializationState::Declared);
                    }
                }
                for statement in &block.statements {
                    self.statement(statement, local, outer, false);
                }
                *local = original;
            }
            Expression::Product(product) => {
                for element in &product.elements {
                    self.expression(&element.value, local, outer);
                }
            }
            Expression::Call(call) => {
                self.expression(&call.callee, local, outer);
                self.expression(&call.argument, local, outer);
            }
            Expression::Access(access) => {
                if let Some(symbol) = self.module.symbol_for(access.syntax.id) {
                    self.read(
                        symbol,
                        access.syntax.id,
                        access.syntax.span.clone(),
                        local,
                        outer,
                    );
                } else {
                    self.expression(&access.value, local, outer);
                }
            }
            Expression::Index(index) => {
                self.expression(&index.value, local, outer);
                self.expression(&index.index, local, outer);
            }
            Expression::Infix(infix) => {
                if let Some(lowered) = self.module.lowered_infix(infix.syntax.id).cloned() {
                    self.expression(&lowered, local, outer);
                } else {
                    for operand in &infix.operands {
                        self.expression(operand, local, outer);
                    }
                }
            }
            Expression::Name(name) => {
                if let Some(symbol) = self.module.symbol_for(name.syntax.id) {
                    self.read(
                        symbol,
                        name.syntax.id,
                        name.syntax.span.clone(),
                        local,
                        outer,
                    );
                }
            }
            Expression::Quote(_)
            | Expression::Splice(_)
            | Expression::String(_)
            | Expression::CString(_)
            | Expression::Integer(_) => {}
        }
    }

    fn read(
        &mut self,
        symbol: SymbolId,
        syntax: SyntaxId,
        span: Span,
        local: &HashMap<SymbolId, InitializationState>,
        outer: &HashMap<SymbolId, InitializationState>,
    ) {
        if let Some(state) = local.get(&symbol) {
            if *state != InitializationState::Initialized && self.diagnosed_reads.insert(syntax) {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    "binding is read before it is initialized",
                ));
            }
        } else if outer
            .get(&symbol)
            .is_some_and(|state| *state != InitializationState::Initialized)
        {
            self.checked_symbols.insert(symbol);
            self.checked_reads.insert(syntax);
        }
    }

    fn set_pattern_state(
        &self,
        pattern: &Pattern,
        state: InitializationState,
        states: &mut HashMap<SymbolId, InitializationState>,
    ) {
        match pattern {
            Pattern::Wildcard(_) => {}
            Pattern::Binding(binding) => {
                if let Some(symbol) = self.module.symbol_for(binding.syntax.id) {
                    states.insert(symbol, state);
                }
            }
            Pattern::Product(product) => {
                for element in &product.elements {
                    self.set_pattern_state(element, state, states);
                }
            }
            Pattern::Nominal(pattern) => self.set_pattern_state(&pattern.argument, state, states),
        }
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
