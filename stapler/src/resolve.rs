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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DefinitionId {
    Symbol(SymbolId),
    Type(TypeId),
    TypeParameter(TypeParameterId),
    Trait(TraitId),
    TraitMethod(TraitMethodId),
    Macro(MacroId),
    Module(ModuleId),
}

#[derive(Debug, Clone)]
pub struct ResolvedTrait {
    pub id: TraitId,
    pub declaration: crate::TraitDeclaration,
    pub parameters: Vec<TypeParameterId>,
    pub methods: Vec<TraitMethodId>,
    pub default_methods: HashMap<TraitMethodId, FunctionId>,
}

#[derive(Debug, Clone)]
pub struct ResolvedTraitImplementation {
    pub syntax: SyntaxId,
    pub trait_id: TraitId,
    pub arguments: Vec<Type>,
    pub methods: HashMap<TraitMethodId, FunctionId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltinType {
    Integer(IntegerType),
    Float(FloatType),
    String,
    Ref,
    CChar,
    CString,
    CPointer,
    IO,
    Syntax,
}

/// Describes a compiler-owned construction strategy that may be entered while
/// the constructed type is still being resolved.
///
/// This is declaration metadata rather than a trait obligation: construction
/// participates in type resolution itself, so resolving a trait implementation
/// first would be circular.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecursiveConstruction {
    ManagedReference,
    Syntax,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FloatType {
    F32,
    F64,
}

impl FloatType {
    pub const ALL: [Self; 2] = [Self::F32, Self::F64];

    pub fn name(self) -> &'static str {
        match self {
            Self::F32 => "F32",
            Self::F64 => "F64",
        }
    }

    pub fn intrinsic_name(self) -> &'static str {
        match self {
            Self::F32 => "f32",
            Self::F64 => "f64",
        }
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedMacro {
    pub declaration: SyntaxId,
    pub name: String,
    pub modifier: bool,
    pub signature: String,
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
    FloatBinary {
        float: FloatType,
        operation: IntegerBinaryOperation,
    },
    FloatCompare {
        float: FloatType,
        operation: IntegerCompareOperation,
    },
    StringFromCString,
    StringToCString,
    ErasedProductLength,
    RefReplace,
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
    type_parameter_sized: HashMap<TypeParameterId, bool>,
    type_parameter_declarations: HashMap<TypeParameterId, SyntaxId>,
    type_parameter_modules: HashMap<TypeParameterId, ModuleId>,
    type_declarations: HashMap<TypeId, TypeDeclaration>,
    type_names: HashMap<TypeId, String>,
    syntax_modules: HashMap<SyntaxId, ModuleId>,
    lowered_infix: HashMap<SyntaxId, Expression>,
    builtin_types: HashMap<TypeId, BuiltinType>,
    recursive_constructions: HashMap<TypeId, RecursiveConstruction>,
    intrinsic_functions: HashMap<SymbolId, IntrinsicFunction>,
    external_symbols: HashSet<SymbolId>,
    macro_calls: HashMap<SyntaxId, PrimitiveMacro>,
    macro_definitions: HashMap<SyntaxId, ResolvedMacro>,
    macro_invocations: HashMap<SyntaxId, ResolvedMacro>,
    macro_declarations: HashMap<MacroId, SyntaxId>,
    macro_modules: HashMap<MacroId, ModuleId>,
    constructors: HashMap<SymbolId, TypeId>,
    singleton_values: HashMap<SymbolId, TypeId>,
    type_modules: HashMap<TypeId, ModuleId>,
    traits: HashMap<TraitId, ResolvedTrait>,
    trait_methods: HashMap<TraitMethodId, crate::TraitMember>,
    trait_method_traits: HashMap<TraitMethodId, TraitId>,
    trait_references: HashMap<SyntaxId, TraitId>,
    trait_method_references: HashMap<SyntaxId, Vec<TraitMethodId>>,
    trait_implementations: Vec<ResolvedTraitImplementation>,
    standard_traits: HashMap<String, TraitId>,
    checked_initialization_symbols: HashSet<SymbolId>,
    checked_initialization_reads: HashSet<SyntaxId>,
    mutable_symbols: HashSet<SymbolId>,
    symbol_modules: HashMap<SymbolId, ModuleId>,
    symbol_declarations: HashMap<SymbolId, SyntaxId>,
    trait_modules: HashMap<TraitId, ModuleId>,
    import_definitions: HashMap<(SyntaxId, String), Vec<DefinitionId>>,
    visible_module_definitions: Vec<HashMap<String, Vec<DefinitionId>>>,
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

    pub fn macro_definition_for(&self, syntax_id: SyntaxId) -> Option<&ResolvedMacro> {
        self.macro_definitions.get(&syntax_id)
    }

    pub fn macro_invocation_for(&self, syntax_id: SyntaxId) -> Option<&ResolvedMacro> {
        self.macro_invocations.get(&syntax_id)
    }

    pub fn macro_for(&self, id: MacroId) -> Option<&ResolvedMacro> {
        self.macro_declarations
            .get(&id)
            .and_then(|syntax| self.macro_definitions.get(syntax))
    }

    pub fn definitions_for(&self, syntax_id: SyntaxId) -> Vec<DefinitionId> {
        let mut definitions = Vec::new();
        if let Some(symbol) = self.symbol_for(syntax_id) {
            if let Some(ty) = self
                .constructor_type(symbol)
                .or_else(|| self.singleton_type(symbol))
            {
                definitions.push(DefinitionId::Type(ty));
            } else {
                definitions.push(DefinitionId::Symbol(symbol));
            }
        }
        if let Some(ty) = self
            .type_for(syntax_id)
            .or_else(|| self.type_for_pattern(syntax_id))
        {
            definitions.push(DefinitionId::Type(ty));
        }
        if let Some(parameter) = self.type_parameter_for(syntax_id) {
            definitions.push(DefinitionId::TypeParameter(parameter));
        }
        if let Some(trait_id) = self.trait_for(syntax_id) {
            definitions.push(DefinitionId::Trait(trait_id));
        }
        definitions.extend(
            self.trait_methods_for_expression(syntax_id)
                .iter()
                .copied()
                .map(DefinitionId::TraitMethod),
        );
        for (id, declaration) in &self.type_declarations {
            if declaration.syntax.id == syntax_id {
                definitions.push(DefinitionId::Type(*id));
            }
        }
        for (id, resolved_trait) in &self.traits {
            if resolved_trait.declaration.syntax.id == syntax_id {
                definitions.push(DefinitionId::Trait(*id));
            }
        }
        for (id, member) in &self.trait_methods {
            if member.syntax.id == syntax_id {
                definitions.push(DefinitionId::TraitMethod(*id));
            }
        }
        for (id, declaration) in &self.macro_declarations {
            if *declaration == syntax_id {
                definitions.push(DefinitionId::Macro(*id));
            }
        }
        let mut seen = HashSet::new();
        definitions.retain(|definition| seen.insert(*definition));
        definitions
    }

    pub fn import_definitions(&self, syntax: SyntaxId, name: &str) -> &[DefinitionId] {
        self.import_definitions
            .get(&(syntax, name.to_owned()))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn visible_definitions(
        &self,
        module: ModuleId,
    ) -> Option<&HashMap<String, Vec<DefinitionId>>> {
        self.visible_module_definitions.get(module.0)
    }

    pub fn declaration_syntax(&self, definition: DefinitionId) -> Option<SyntaxId> {
        match definition {
            DefinitionId::Symbol(symbol) => self.symbol_declarations.get(&symbol).copied(),
            DefinitionId::Type(id) => self.type_declarations.get(&id).map(|value| value.syntax.id),
            DefinitionId::TypeParameter(id) => self.type_parameter_declarations.get(&id).copied(),
            DefinitionId::Trait(id) => self
                .traits
                .get(&id)
                .map(|value| value.declaration.syntax.id),
            DefinitionId::TraitMethod(id) => {
                self.trait_methods.get(&id).map(|value| value.syntax.id)
            }
            DefinitionId::Macro(id) => self.macro_declarations.get(&id).copied(),
            DefinitionId::Module(id) => self.program.modules().iter().find_map(|module| {
                module.syntax.items.iter().find_map(|item| {
                    let Item::Submodule(submodule) = item else {
                        return None;
                    };
                    (self.program.child_module(submodule.syntax.id) == Some(id))
                        .then_some(submodule.syntax.id)
                })
            }),
        }
    }

    pub fn definition_module(&self, definition: DefinitionId) -> Option<ModuleId> {
        match definition {
            DefinitionId::Symbol(symbol) => self.symbol_module(symbol),
            DefinitionId::Type(id) => self.type_modules.get(&id).copied(),
            DefinitionId::TypeParameter(id) => self.type_parameter_modules.get(&id).copied(),
            DefinitionId::Trait(id) => self.trait_modules.get(&id).copied(),
            DefinitionId::TraitMethod(id) => self
                .trait_for_method(id)
                .and_then(|trait_id| self.trait_modules.get(&trait_id).copied()),
            DefinitionId::Macro(id) => self.macro_modules.get(&id).copied(),
            DefinitionId::Module(id) => Some(id),
        }
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

    pub fn type_parameter_is_sized(&self, id: TypeParameterId) -> bool {
        self.type_parameter_sized.get(&id).copied().unwrap_or(true)
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

    pub fn recursive_construction(&self, id: TypeId) -> Option<RecursiveConstruction> {
        self.recursive_constructions.get(&id).copied()
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

    pub fn standard_trait(&self, name: &str) -> Option<TraitId> {
        self.standard_traits.get(name).copied()
    }

    pub fn requires_initialization_state(&self, symbol: SymbolId) -> bool {
        self.checked_initialization_symbols.contains(&symbol)
    }

    pub fn requires_initialization_check(&self, syntax: SyntaxId) -> bool {
        self.checked_initialization_reads.contains(&syntax)
    }

    pub fn is_mutable_symbol(&self, symbol: SymbolId) -> bool {
        self.mutable_symbols.contains(&symbol)
    }

    pub fn symbol_module(&self, symbol: SymbolId) -> Option<ModuleId> {
        self.symbol_modules.get(&symbol).copied()
    }
}

#[derive(Clone, Default)]
struct Interface {
    values: HashMap<String, SymbolId>,
    fixities: HashMap<String, Fixity>,
    types: HashMap<String, TypeId>,
    macros: HashMap<String, Vec<MacroId>>,
    traits: HashMap<String, TraitId>,
}

fn is_ancestor(program: &Program, ancestor: ModuleId, mut module: ModuleId) -> bool {
    while let Some(parent) = program.parent_module(module) {
        if parent == ancestor {
            return true;
        }
        module = parent;
    }
    false
}

fn add_private_candidate(candidates: &mut HashMap<String, Vec<String>>, name: &str, module: &str) {
    let modules = candidates.entry(name.to_owned()).or_default();
    if !modules.iter().any(|candidate| candidate == module) {
        modules.push(module.to_owned());
    }
}

fn unknown_item_message(
    kind: &str,
    name: &str,
    candidates: &HashMap<String, Vec<String>>,
) -> String {
    let base = format!("unknown {kind} `{name}`");
    let Some(modules) = candidates.get(name) else {
        return base;
    };
    if let [module] = modules.as_slice() {
        format!("{base}; `{name}` exists in module `{module}`, but it is private")
    } else {
        let modules = modules
            .iter()
            .map(|module| format!("`{module}`"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{base}; private items named `{name}` exist in modules {modules}")
    }
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
    if let Some(macro_ids) = imported.macros.get(item) {
        changed |= exported
            .macros
            .insert(alias.to_owned(), macro_ids.clone())
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
    imported_macros: HashMap<String, Vec<MacroId>>,
    imported_traits: HashMap<String, TraitId>,
    private_glob_values: HashMap<String, Vec<String>>,
    private_glob_types: HashMap<String, Vec<String>>,
    private_glob_traits: HashMap<String, Vec<String>>,
    visible_trait_methods: HashMap<String, Vec<TraitMethodId>>,
    type_parameter_scopes: Vec<HashMap<String, TypeParameterId>>,
    prelude_values: HashMap<String, SymbolId>,
    prelude_types: HashMap<String, TypeId>,
    prelude_fixities: HashMap<String, Fixity>,
    prelude_macros: HashMap<String, Vec<MacroId>>,
    prelude_traits: HashMap<String, TraitId>,
    symbols: HashMap<SyntaxId, SymbolId>,
    function_expressions: HashMap<SyntaxId, FunctionId>,
    symbol_owners: HashMap<SymbolId, Option<FunctionId>>,
    function_parents: HashMap<FunctionId, Option<FunctionId>>,
    function_captures: HashMap<FunctionId, Vec<SymbolId>>,
    function_stack: Vec<FunctionId>,
    loop_depth: usize,
    declared_fixities: Vec<HashMap<String, Fixity>>,
    lowered_infix: HashMap<SyntaxId, Expression>,
    builtin_types: HashMap<TypeId, BuiltinType>,
    recursive_constructions: HashMap<TypeId, RecursiveConstruction>,
    intrinsic_functions: HashMap<SymbolId, IntrinsicFunction>,
    primitive_macros: HashMap<MacroId, PrimitiveMacro>,
    macro_calls: HashMap<SyntaxId, PrimitiveMacro>,
    macro_declarations: HashMap<MacroId, SyntaxId>,
    macro_modules: HashMap<MacroId, ModuleId>,
    constructors: HashMap<SymbolId, TypeId>,
    singleton_values: HashMap<SymbolId, TypeId>,
    nominal_patterns: HashMap<SyntaxId, TypeId>,
    type_modules: HashMap<TypeId, ModuleId>,
    type_constructor_symbols: HashMap<TypeId, SymbolId>,
    functions: Vec<ResolvedFunction>,
    named_types: HashMap<SyntaxId, TypeId>,
    type_parameters: HashMap<SyntaxId, TypeParameterId>,
    type_parameter_sized: HashMap<TypeParameterId, bool>,
    type_parameter_declarations: HashMap<TypeParameterId, SyntaxId>,
    type_parameter_modules: HashMap<TypeParameterId, ModuleId>,
    type_declarations: HashMap<TypeId, TypeDeclaration>,
    type_names: HashMap<TypeId, String>,
    traits: HashMap<TraitId, ResolvedTrait>,
    trait_methods: HashMap<TraitMethodId, crate::TraitMember>,
    trait_method_traits: HashMap<TraitMethodId, TraitId>,
    trait_member_ids: HashMap<(TraitId, String), TraitMethodId>,
    trait_modules: HashMap<TraitId, ModuleId>,
    trait_references: HashMap<SyntaxId, TraitId>,
    trait_method_references: HashMap<SyntaxId, Vec<TraitMethodId>>,
    trait_implementations: Vec<ResolvedTraitImplementation>,
    syntax_modules: HashMap<SyntaxId, ModuleId>,
    mutable_symbols: HashSet<SymbolId>,
    symbol_modules: HashMap<SymbolId, ModuleId>,
    import_definitions: HashMap<(SyntaxId, String), Vec<DefinitionId>>,
    visible_module_definitions: Vec<HashMap<String, Vec<DefinitionId>>>,
    interfaces: Vec<Interface>,
    declared_symbols: HashMap<SyntaxId, SymbolId>,
    symbol_declarations: HashMap<SymbolId, SyntaxId>,
    module_values: Vec<HashMap<String, SymbolId>>,
    definition_context_values: Vec<HashMap<String, SymbolId>>,
    definition_context_types: Vec<HashMap<String, TypeId>>,
    definition_context_namespaces: Vec<HashMap<String, ModuleId>>,
    definition_context_fixities: Vec<HashMap<String, Fixity>>,
    binding_type_parameters: HashMap<SyntaxId, Vec<TypeParameterPattern>>,
    binding_trait_bounds: HashMap<SyntaxId, Vec<crate::TraitBound>>,
    declared_types: Vec<HashMap<String, TypeId>>,
    declared_macros: Vec<HashMap<String, Vec<MacroId>>>,
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
    standard_library_io: Option<ModuleId>,
}

impl NameResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn resolve(self, module: &Module) -> Result<ResolvedModule, Vec<Diagnostic>> {
        self.resolve_program(Program::single(module.clone()))
    }

    pub fn resolve_program(mut self, program: Program) -> Result<ResolvedModule, Vec<Diagnostic>> {
        let (program, macro_analysis) = crate::macro_expand::expand_program(program)?;
        self.standard_library_core = program.standard_library_core();
        self.standard_library_cinterop = program.standard_library_cinterop();
        self.standard_library_io = program.standard_library_io();
        let standard_library_directory = self
            .standard_library_core
            .and_then(|core| program.module(core).path.parent());
        self.multiple_modules = program
            .modules()
            .iter()
            .filter(|module| {
                standard_library_directory
                    .is_none_or(|directory| !module.path.starts_with(directory))
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
        self.visible_module_definitions = vec![HashMap::new(); program.modules().len()];
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
            self.private_glob_values.clear();
            self.private_glob_types.clear();
            self.private_glob_traits.clear();
            self.visible_trait_methods.clear();
            self.prelude_values.clear();
            self.prelude_types.clear();
            self.prelude_fixities.clear();
            self.prelude_macros.clear();
            self.prelude_traits.clear();
            self.push_scope();
            self.install_child_namespaces(&program, source_module.id);
            self.install_prelude(&program, source_module.id);
            self.install_imports(&program, source_module.id);
            self.install_local_traits(source_module.id);
            self.predeclare_items(&source_module.syntax.items);
            self.record_visible_module_definitions(source_module.id);
            for item in &source_module.syntax.items {
                self.resolve_item(item);
            }
            self.pop_scope();
        }

        self.validate_trait_prerequisite_cycles();

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
        let standard_traits = self
            .standard_library_core
            .map(|core| self.interfaces[core.0].traits.clone())
            .unwrap_or_default();
        let mut resolved = ResolvedModule {
            program,
            functions: self.functions,
            symbols: self.symbols,
            function_expressions: self.function_expressions,
            named_types: self.named_types,
            nominal_patterns: self.nominal_patterns,
            type_parameters: self.type_parameters,
            type_parameter_sized: self.type_parameter_sized,
            type_parameter_declarations: self.type_parameter_declarations,
            type_parameter_modules: self.type_parameter_modules,
            type_declarations: self.type_declarations,
            type_names: self.type_names,
            syntax_modules: self.syntax_modules,
            lowered_infix: self.lowered_infix,
            builtin_types: self.builtin_types,
            recursive_constructions: self.recursive_constructions,
            intrinsic_functions: self.intrinsic_functions,
            external_symbols,
            macro_calls: self.macro_calls,
            macro_definitions: macro_analysis.definitions,
            macro_invocations: macro_analysis.invocations,
            macro_declarations: self.macro_declarations,
            macro_modules: self.macro_modules,
            constructors: self.constructors,
            singleton_values: self.singleton_values,
            type_modules: self.type_modules,
            traits: self.traits,
            trait_methods: self.trait_methods,
            trait_method_traits: self.trait_method_traits,
            trait_modules: self.trait_modules,
            trait_references: self.trait_references,
            trait_method_references: self.trait_method_references,
            trait_implementations: self.trait_implementations,
            standard_traits,
            checked_initialization_symbols: HashSet::new(),
            checked_initialization_reads: HashSet::new(),
            mutable_symbols: self.mutable_symbols,
            symbol_modules: self.symbol_modules,
            symbol_declarations: self.symbol_declarations,
            import_definitions: self.import_definitions,
            visible_module_definitions: self.visible_module_definitions,
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
        for float in FloatType::ALL {
            self.register_builtin_type(core, "std.core", float.name(), BuiltinType::Float(float));
        }
        self.register_builtin_type(core, "std.core", "String", BuiltinType::String);
        self.register_builtin_type(core, "std.core", "Ref", BuiltinType::Ref);
        for name in [
            "Ident",
            "CallExpr",
            "Sequence",
            "Optional",
            "Separated",
            "Comma",
            "Parenthesized",
            "Bracketed",
            "Braced",
            "UnstructuredExpr",
            "Expr",
            "Type",
            "Pattern",
            "Item",
            "Syntax",
        ] {
            self.register_builtin_type(core, "std.core", name, BuiltinType::Syntax);
        }

        if let Some(cinterop) = program.standard_library_cinterop() {
            self.register_builtin_type(cinterop, "std.cinterop", "CChar", BuiltinType::CChar);
            self.register_builtin_type(cinterop, "std.cinterop", "CString", BuiltinType::CString);
            self.register_builtin_type(cinterop, "std.cinterop", "CPointer", BuiltinType::CPointer);
            self.register_primitive_macro(cinterop, "c_string", PrimitiveMacro::CString);
        }
        if let Some(io) = program.standard_library_io() {
            self.register_builtin_type(io, "std.io", "IO", BuiltinType::IO);
        }

        for (id, declaration) in &self.type_declarations {
            if declaration.recursive_constructor && !self.recursive_constructions.contains_key(id) {
                self.diagnostics.push(Diagnostic::new(
                    declaration.syntax.span.clone(),
                    "`@recursive_constructor` may only mark a compiler-owned recursive constructor",
                ));
            }
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
                ("lt", IntegerCompareOperation::LessThan),
                ("gt", IntegerCompareOperation::GreaterThan),
            ] {
                expected.push((
                    format!("__{}_{}", integer.intrinsic_name(), suffix),
                    IntrinsicFunction::IntegerCompare { integer, operation },
                ));
            }
        }
        for float in FloatType::ALL {
            for (suffix, operation) in [
                ("add", IntegerBinaryOperation::Add),
                ("subtract", IntegerBinaryOperation::Subtract),
                ("multiply", IntegerBinaryOperation::Multiply),
                ("divide", IntegerBinaryOperation::Divide),
            ] {
                expected.push((
                    format!("__{}_{}", float.intrinsic_name(), suffix),
                    IntrinsicFunction::FloatBinary { float, operation },
                ));
            }
            for (suffix, operation) in [
                ("equal", IntegerCompareOperation::Equal),
                ("not_equal", IntegerCompareOperation::NotEqual),
                ("lt", IntegerCompareOperation::LessThan),
                ("gt", IntegerCompareOperation::GreaterThan),
            ] {
                expected.push((
                    format!("__{}_{}", float.intrinsic_name(), suffix),
                    IntrinsicFunction::FloatCompare { float, operation },
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
        let standard_library_directory = program
            .module(core)
            .path
            .parent()
            .expect("std.core has a parent directory");
        for source_module in program
            .modules()
            .iter()
            .filter(|module| module.path.starts_with(standard_library_directory))
        {
            for item in &source_module.syntax.items {
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
        if let Some(symbol) = self.interfaces[core.0].values.get("drop").copied() {
            self.intrinsic_functions
                .insert(symbol, IntrinsicFunction::Drop);
        }
        if let Some(symbol) = self.interfaces[core.0].values.get("length").copied() {
            self.intrinsic_functions
                .insert(symbol, IntrinsicFunction::ErasedProductLength);
        }
        if let Some(symbol) = self.interfaces[core.0].values.get("replace").copied() {
            self.intrinsic_functions
                .insert(symbol, IntrinsicFunction::RefReplace);
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
            if source_module.path.starts_with(standard_library_directory) {
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
        let Some(ids) = self.declared_macros[module.0].get(name).cloned() else {
            self.diagnostics.push(Diagnostic::new(
                Span::Compiler,
                format!("standard library `std.cinterop` does not declare macro `{name}`"),
            ));
            return;
        };
        let [id] = ids.as_slice() else {
            self.diagnostics.push(Diagnostic::new(
                Span::Compiler,
                format!("compiler-provided macro `{name}` cannot be overloaded"),
            ));
            return;
        };
        if !self.interfaces[module.0].macros.contains_key(name) {
            self.diagnostics.push(Diagnostic::new(
                Span::Compiler,
                format!("standard library macro `{name}` must be public"),
            ));
        }
        self.primitive_macros.insert(*id, primitive);
    }

    fn register_builtin_type(
        &mut self,
        module: ModuleId,
        module_name: &str,
        name: &str,
        builtin: BuiltinType,
    ) {
        let Some(id) = self.interfaces[module.0].types.get(name).copied() else {
            self.diagnostics.push(Diagnostic::new(
                Span::Compiler,
                format!("standard library `{module_name}` does not export `{name}`"),
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
        if builtin == BuiltinType::String {
            if declaration.kind != crate::TypeDeclarationKind::Distinct {
                self.diagnostics.push(Diagnostic::new(
                    declaration.syntax.span.clone(),
                    "standard library type `String` must be a represented distinct type",
                ));
            }
            if declaration.representation_visibility != Visibility::Private {
                self.diagnostics.push(Diagnostic::new(
                    declaration.syntax.span.clone(),
                    "standard library type `String` must keep its representation private",
                ));
            }
            if !declaration.type_parameters.is_empty() {
                self.diagnostics.push(Diagnostic::new(
                    declaration.syntax.span.clone(),
                    "standard library type `String` must not accept compile-time arguments",
                ));
            }
        } else if builtin != BuiltinType::Syntax {
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
        }
        if matches!(builtin, BuiltinType::CPointer | BuiltinType::Ref)
            && declaration.type_parameters.len() != 1
        {
            self.diagnostics.push(Diagnostic::new(
                declaration.syntax.span.clone(),
                format!("standard library type `{name}` must accept one compile-time argument"),
            ));
        }
        if builtin == BuiltinType::IO && !declaration.type_parameters.is_empty() {
            self.diagnostics.push(Diagnostic::new(
                declaration.syntax.span.clone(),
                "standard library type `IO` must not accept compile-time arguments",
            ));
        }
        if builtin == BuiltinType::Ref
            && !matches!(
                declaration.type_parameters.as_slice(),
                [TypeParameterPattern::Binding(binding)] if !binding.sized
            )
        {
            self.diagnostics.push(Diagnostic::new(
                declaration.syntax.span.clone(),
                "standard library type `Ref` must relax its parameter with `?Sized`",
            ));
        }
        let recursive_construction = match builtin {
            BuiltinType::Ref => Some(RecursiveConstruction::ManagedReference),
            BuiltinType::Syntax if declaration.kind == crate::TypeDeclarationKind::Distinct => {
                Some(RecursiveConstruction::Syntax)
            }
            _ => None,
        };
        match (declaration.recursive_constructor, recursive_construction) {
            (true, Some(construction)) => {
                self.recursive_constructions.insert(id, construction);
            }
            (false, Some(_)) => self.diagnostics.push(Diagnostic::new(
                declaration.syntax.span.clone(),
                format!("standard library type `{name}` must be marked `@recursive_constructor`"),
            )),
            (true, None) => self.diagnostics.push(Diagnostic::new(
                declaration.syntax.span.clone(),
                format!("standard library type `{name}` cannot be marked `@recursive_constructor`"),
            )),
            (false, None) => {}
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
            self.current_module = source_module.id;
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
                        self.macro_declarations.insert(id, declaration.syntax.id);
                        self.macro_modules.insert(id, source_module.id);
                        self.declared_macros[source_module.id.0]
                            .entry(declaration.name.clone())
                            .or_default()
                            .push(id);
                        if declaration.visibility == Visibility::Public {
                            self.interfaces[source_module.id.0]
                                .macros
                                .entry(declaration.name.clone())
                                .or_default()
                                .push(id);
                        }
                    }
                    Item::TraitDeclaration(declaration) => {
                        let id = TraitId(self.next_trait_id);
                        self.next_trait_id += 1;
                        self.trait_modules.insert(id, source_module.id);
                        if self.declared_traits[source_module.id.0]
                            .insert(declaration.name.clone(), id)
                            .is_some()
                        {
                            self.diagnostics.push(Diagnostic::new(
                                declaration.syntax.span.clone(),
                                format!("duplicate trait definition of `{}`", declaration.name),
                            ));
                        }
                        let mut parameters = Vec::new();
                        for pattern in &declaration.type_parameters {
                            self.allocate_type_parameter_pattern(pattern, &mut parameters);
                        }
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
                                parameters,
                                methods,
                                default_methods: HashMap::new(),
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
                        Statement::Assignment(_)
                        | Statement::Return(_)
                        | Statement::Break(_)
                        | Statement::Continue(_)
                        | Statement::Expression(_) => {}
                    },
                    Item::Modified(_)
                    | Item::VisibilityMacroInvocation(_)
                    | Item::VisibilitySplice(_)
                    | Item::RepeatedItemSplice(_)
                    | Item::UseDeclaration(_)
                    | Item::Submodule(_) => {}
                }
            }
        }
        self.collect_reexports(program);
    }

    fn build_definition_context_values(&mut self, program: &Program) {
        self.definition_context_values = self.module_values.clone();
        self.definition_context_types = self.declared_types.clone();
        self.definition_context_fixities = self.declared_fixities.clone();
        self.definition_context_namespaces = (0..program.modules().len())
            .map(|_| HashMap::new())
            .collect();
        for module in program.modules() {
            for item in &module.syntax.items {
                if let Item::Submodule(submodule) = item
                    && let Some(child) = program.child_module(submodule.syntax.id)
                {
                    self.definition_context_namespaces[module.id.0]
                        .insert(submodule.name.clone(), child);
                }
            }
        }
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
                let interface = if use_.visibility == Visibility::Private
                    && is_ancestor(program, imported, module.id)
                {
                    self.local_interface(imported)
                } else {
                    self.interfaces[imported.0].clone()
                };
                match &use_.kind {
                    UseKind::Glob => {
                        self.definition_context_values[module.id.0]
                            .extend(interface.values.clone());
                        self.definition_context_types[module.id.0].extend(interface.types.clone());
                        self.definition_context_fixities[module.id.0]
                            .extend(interface.fixities.clone());
                    }
                    UseKind::Selected(names) => {
                        for name in names {
                            if let Some(symbol) = interface.values.get(name) {
                                self.definition_context_values[module.id.0]
                                    .insert(name.clone(), *symbol);
                            }
                            if let Some(ty) = interface.types.get(name) {
                                self.definition_context_types[module.id.0]
                                    .insert(name.clone(), *ty);
                            }
                            if let Some(fixity) = interface.fixities.get(name) {
                                self.definition_context_fixities[module.id.0]
                                    .insert(name.clone(), *fixity);
                            }
                        }
                    }
                    UseKind::Renamed { item, alias } => {
                        if let Some(symbol) = interface.values.get(item) {
                            self.definition_context_values[module.id.0]
                                .insert(alias.clone(), *symbol);
                        }
                        if let Some(ty) = interface.types.get(item) {
                            self.definition_context_types[module.id.0].insert(alias.clone(), *ty);
                        }
                        if let Some(fixity) = interface.fixities.get(item) {
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
        self.symbol_declarations.insert(symbol, binding.syntax.id);
        self.binding_type_parameters
            .insert(binding.syntax.id, binding.type_parameters.clone());
        self.binding_trait_bounds
            .insert(binding.syntax.id, binding.trait_bounds.clone());
        self.symbols.insert(binding.syntax.id, symbol);
        self.symbol_owners.insert(symbol, None);
        self.symbol_modules.insert(symbol, self.current_module);
        if binding.mutable {
            self.mutable_symbols.insert(symbol);
        }
        symbol
    }

    fn allocate_pattern_symbols(&mut self, pattern: &Pattern) {
        match pattern {
            Pattern::Wildcard(_) | Pattern::StringLiteral(_) | Pattern::Splice(_) => {}
            Pattern::Binding(binding) => {
                let symbol = SymbolId(self.next_symbol_id);
                self.next_symbol_id += 1;
                self.declared_symbols.insert(binding.syntax.id, symbol);
                self.symbol_declarations.insert(symbol, binding.syntax.id);
                self.symbols.insert(binding.syntax.id, symbol);
                self.symbol_owners.insert(symbol, None);
                self.symbol_modules.insert(symbol, self.current_module);
                if binding.mutable {
                    self.mutable_symbols.insert(symbol);
                }
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
            let interface = if declaration.visibility == Visibility::Private
                && is_ancestor(program, imported, module)
            {
                self.local_interface(imported)
            } else {
                self.interfaces[imported.0].clone()
            };
            if declaration.kind == UseKind::Glob {
                self.record_private_glob_items(imported, declaration, &interface);
            }
            if let Some(name) = declaration.path.last() {
                self.import_definitions
                    .entry((declaration.syntax.id, name.clone()))
                    .or_default()
                    .push(DefinitionId::Module(imported));
            }
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
                    for (name, symbol) in interface.values.clone() {
                        if let Some(fixity) = interface.fixities.get(&name).copied() {
                            self.imported_fixities.insert(name.clone(), fixity);
                        }
                        self.insert_imported_value(name, symbol, declaration.syntax.span.clone());
                    }
                    for (name, ty) in interface.types.clone() {
                        self.insert_imported_type(name, ty, declaration.syntax.span.clone());
                    }
                    for (name, macro_id) in interface.macros.clone() {
                        self.insert_imported_macro(name, macro_id, declaration.syntax.span.clone());
                    }
                    for (name, trait_id) in interface.traits.clone() {
                        self.insert_imported_trait(name, trait_id, declaration.syntax.span.clone());
                    }
                }
                UseKind::Selected(names) => {
                    for name in names {
                        self.record_import_definitions(
                            declaration.syntax.id,
                            name,
                            name,
                            &interface,
                        );
                        self.install_selected(
                            &interface,
                            name,
                            name,
                            declaration.syntax.span.clone(),
                        );
                    }
                }
                UseKind::Renamed { item, alias } => {
                    self.record_import_definitions(declaration.syntax.id, item, alias, &interface);
                    self.install_selected(&interface, item, alias, declaration.syntax.span.clone());
                }
            }
        }
    }

    fn record_import_definitions(
        &mut self,
        syntax: SyntaxId,
        item: &str,
        alias: &str,
        interface: &Interface,
    ) {
        let mut definitions = Vec::new();
        if let Some(symbol) = interface.values.get(item).copied() {
            if let Some(ty) = self
                .constructors
                .get(&symbol)
                .or_else(|| self.singleton_values.get(&symbol))
                .copied()
            {
                definitions.push(DefinitionId::Type(ty));
            } else {
                definitions.push(DefinitionId::Symbol(symbol));
            }
        }
        if let Some(ty) = interface.types.get(item).copied() {
            definitions.push(DefinitionId::Type(ty));
        }
        if let Some(trait_id) = interface.traits.get(item).copied() {
            definitions.push(DefinitionId::Trait(trait_id));
        }
        if let Some(macros) = interface.macros.get(item) {
            definitions.extend(macros.iter().copied().map(DefinitionId::Macro));
        }
        let mut seen = HashSet::new();
        definitions.retain(|definition| seen.insert(*definition));
        for name in [item, alias] {
            self.import_definitions
                .entry((syntax, name.to_owned()))
                .or_default()
                .extend(definitions.iter().copied());
        }
    }

    fn install_child_namespaces(&mut self, program: &Program, module: ModuleId) {
        for item in &program.module(module).syntax.items {
            let Item::Submodule(submodule) = item else {
                continue;
            };
            let Some(child) = program.child_module(submodule.syntax.id) else {
                continue;
            };
            if self
                .namespaces
                .insert(submodule.name.clone(), child)
                .is_some()
            {
                self.duplicate_import(&submodule.name, submodule.syntax.span.clone());
            }
        }
    }

    fn local_interface(&self, module: ModuleId) -> Interface {
        Interface {
            values: self.module_values[module.0].clone(),
            fixities: self.declared_fixities[module.0].clone(),
            types: self.declared_types[module.0].clone(),
            macros: self.declared_macros[module.0].clone(),
            traits: self.declared_traits[module.0].clone(),
        }
    }

    fn record_private_glob_items(
        &mut self,
        module: ModuleId,
        declaration: &crate::UseDeclaration,
        imported: &Interface,
    ) {
        let local = self.local_interface(module);
        let module_name = declaration.path.join(".");
        for name in local.values.keys() {
            if !imported.values.contains_key(name) {
                add_private_candidate(&mut self.private_glob_values, name, &module_name);
            }
        }
        for name in local.macros.keys() {
            if !imported.macros.contains_key(name) {
                add_private_candidate(&mut self.private_glob_values, name, &module_name);
            }
        }
        for name in local.types.keys() {
            if !imported.types.contains_key(name) {
                add_private_candidate(&mut self.private_glob_types, name, &module_name);
            }
        }
        for name in local.traits.keys() {
            if !imported.traits.contains_key(name) {
                add_private_candidate(&mut self.private_glob_traits, name, &module_name);
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

    fn install_selected(&mut self, interface: &Interface, item: &str, local: &str, span: Span) {
        let mut found = false;
        if let Some(symbol) = interface.values.get(item).copied() {
            found = true;
            if let Some(fixity) = interface.fixities.get(item).copied() {
                self.imported_fixities.insert(local.to_owned(), fixity);
            }
            self.insert_imported_value(local.to_owned(), symbol, span.clone());
        }
        if let Some(ty) = interface.types.get(item).copied() {
            found = true;
            self.insert_imported_type(local.to_owned(), ty, span.clone());
        }
        if let Some(macro_id) = interface.macros.get(item).cloned() {
            found = true;
            self.insert_imported_macro(local.to_owned(), macro_id, span.clone());
        }
        if let Some(trait_id) = interface.traits.get(item).copied() {
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

    fn insert_imported_macro(&mut self, name: String, id: Vec<MacroId>, span: Span) {
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
                | Item::Modified(_)
                | Item::VisibilityMacroInvocation(_)
                | Item::VisibilitySplice(_)
                | Item::RepeatedItemSplice(_)
                | Item::Submodule(_)
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

    fn record_visible_module_definitions(&mut self, module: ModuleId) {
        let mut visible = HashMap::<String, Vec<DefinitionId>>::new();
        let mut insert = |name: &str, definition: DefinitionId| {
            let definitions = visible.entry(name.to_owned()).or_default();
            if !definitions.contains(&definition) {
                definitions.push(definition);
            }
        };
        for (name, symbol) in self.current_scope() {
            insert(name, DefinitionId::Symbol(*symbol));
        }
        for names in [
            &self.declared_types[module.0],
            &self.imported_types,
            &self.prelude_types,
        ] {
            for (name, id) in names {
                insert(name, DefinitionId::Type(*id));
            }
        }
        for names in [
            &self.declared_traits[module.0],
            &self.imported_traits,
            &self.prelude_traits,
        ] {
            for (name, id) in names {
                insert(name, DefinitionId::Trait(*id));
            }
        }
        for names in [
            &self.declared_macros[module.0],
            &self.imported_macros,
            &self.prelude_macros,
        ] {
            for (name, ids) in names {
                for id in ids {
                    insert(name, DefinitionId::Macro(*id));
                }
            }
        }
        for (name, id) in &self.namespaces {
            insert(name, DefinitionId::Module(*id));
        }
        self.visible_module_definitions[module.0] = visible;
    }

    fn resolve_item(&mut self, item: &Item) {
        match item {
            Item::Modified(_)
            | Item::VisibilityMacroInvocation(_)
            | Item::VisibilitySplice(_)
            | Item::RepeatedItemSplice(_)
            | Item::UseDeclaration(_)
            | Item::Submodule(_) => {}
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
                for bound in &declaration.trait_bounds {
                    if let Some(trait_id) = self.resolve_trait_name(&bound.trait_name) {
                        self.trait_references.insert(bound.syntax.id, trait_id);
                    }
                    for argument in &bound.arguments {
                        self.resolve_type(argument);
                    }
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
                let Some(trait_id) = self.declared_traits[self.current_module.0]
                    .get(&declaration.name)
                    .copied()
                else {
                    return;
                };
                self.push_type_parameter_scope();
                for parameter in &declaration.type_parameters {
                    self.scope_allocated_type_parameter_pattern(parameter);
                }
                for prerequisite in &declaration.prerequisites {
                    if let Some(trait_id) = self.resolve_trait_name(&prerequisite.trait_name) {
                        self.trait_references
                            .insert(prerequisite.syntax.id, trait_id);
                    }
                    for argument in &prerequisite.arguments {
                        self.resolve_type(argument);
                    }
                }
                let mut default_methods = HashMap::new();
                for member in &declaration.members {
                    self.resolve_type(&member.annotation);
                    if declaration.visibility == Visibility::Public {
                        self.validate_public_representation(&member.annotation);
                    }
                    let Some(default) = &member.default else {
                        continue;
                    };
                    self.binding_type_parameters
                        .insert(member.syntax.id, declaration.type_parameters.clone());
                    let function_name = format!("trait.default.{}.{}", trait_id.0, member.name);
                    self.resolve_expression(
                        default,
                        Some(&member.annotation),
                        Some((&function_name, member.syntax.id)),
                    );
                    let method = self.trait_member_ids[&(trait_id, member.name.clone())];
                    if let Some(function) =
                        self.function_expressions.get(&default.syntax().id).copied()
                    {
                        default_methods.insert(method, function);
                    } else {
                        self.diagnostics.push(Diagnostic::new(
                            default.syntax().span.clone(),
                            "default trait member implementations must be function values",
                        ));
                    }
                }
                self.traits
                    .get_mut(&trait_id)
                    .expect("resolved trait")
                    .default_methods = default_methods;
                self.pop_type_parameter_scope();
            }
            Item::TraitImplementation(implementation) => {
                let trait_id = self.resolve_trait_name(&implementation.trait_name);
                for argument in &implementation.arguments {
                    self.resolve_type(argument);
                }
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
                    if let Some(method) = method {
                        self.trait_method_references
                            .insert(member.syntax.id, vec![method]);
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
                            arguments: implementation.arguments.clone(),
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
            Statement::Assignment(assignment) => {
                self.syntax_modules
                    .insert(assignment.syntax.id, self.current_module);
                self.resolve_expression(&assignment.target, None, None);
                self.resolve_expression(&assignment.value, None, None);
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
            Statement::Break(statement) => {
                if self.loop_depth == 0 {
                    self.diagnostics.push(Diagnostic::new(
                        statement.syntax.span.clone(),
                        "`break` is only allowed inside a loop",
                    ));
                }
                if let Some(value) = &statement.value {
                    self.resolve_expression(value, None, None);
                }
            }
            Statement::Continue(statement) => {
                if self.loop_depth == 0 {
                    self.diagnostics.push(Diagnostic::new(
                        statement.syntax.span.clone(),
                        "`continue` is only allowed inside a loop",
                    ));
                }
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
            for argument in &bound.arguments {
                self.resolve_type(argument);
            }
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
            if binding.mutable && binding.value.is_none() {
                self.diagnostics.push(Diagnostic::new(
                    binding.syntax.span.clone(),
                    "mutable `let` bindings require an initializer",
                ));
            }
        }
        self.pop_type_parameter_scope();
    }

    fn validate_trait_prerequisite_cycles(&mut self) {
        let adjacency = self
            .traits
            .iter()
            .map(|(trait_id, resolved_trait)| {
                let prerequisites = resolved_trait
                    .declaration
                    .prerequisites
                    .iter()
                    .filter_map(|prerequisite| {
                        Some((
                            self.trait_references
                                .get(&prerequisite.syntax.id)
                                .copied()?,
                            prerequisite.syntax.span.clone(),
                        ))
                    })
                    .collect::<Vec<_>>();
                (*trait_id, prerequisites)
            })
            .collect::<HashMap<_, _>>();
        let mut traits = adjacency.keys().copied().collect::<Vec<_>>();
        traits.sort_by_key(|trait_id| trait_id.0);
        let mut states = HashMap::<TraitId, u8>::new();

        fn visit(
            trait_id: TraitId,
            adjacency: &HashMap<TraitId, Vec<(TraitId, Span)>>,
            states: &mut HashMap<TraitId, u8>,
            diagnostics: &mut Vec<Diagnostic>,
        ) {
            states.insert(trait_id, 1);
            for (prerequisite, span) in adjacency.get(&trait_id).into_iter().flatten() {
                match states.get(prerequisite).copied().unwrap_or_default() {
                    0 => visit(*prerequisite, adjacency, states, diagnostics),
                    1 => {
                        diagnostics.push(Diagnostic::new(span.clone(), "cyclic trait prerequisite"))
                    }
                    _ => {}
                }
            }
            states.insert(trait_id, 2);
        }

        for trait_id in traits {
            if states.get(&trait_id).copied().unwrap_or_default() == 0 {
                visit(trait_id, &adjacency, &mut states, &mut self.diagnostics);
            }
        }
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
                self.type_parameter_sized.insert(id, binding.sized);
                self.type_parameter_declarations
                    .insert(id, binding.syntax.id);
                self.type_parameter_modules.insert(id, self.current_module);
            }
            TypeParameterPattern::Product(product) => {
                for element in &product.elements {
                    self.declare_type_parameter_pattern(element);
                }
            }
            TypeParameterPattern::Splice(_) => {
                unreachable!("type-parameter splices must be expanded before resolution")
            }
        }
    }

    fn allocate_type_parameter_pattern(
        &mut self,
        pattern: &TypeParameterPattern,
        parameters: &mut Vec<TypeParameterId>,
    ) {
        match pattern {
            TypeParameterPattern::Binding(binding) => {
                let id = TypeParameterId(self.next_type_parameter_id);
                self.next_type_parameter_id += 1;
                self.type_parameters.insert(binding.syntax.id, id);
                self.type_parameter_sized.insert(id, binding.sized);
                self.type_parameter_declarations
                    .insert(id, binding.syntax.id);
                self.type_parameter_modules.insert(id, self.current_module);
                parameters.push(id);
            }
            TypeParameterPattern::Product(product) => {
                for element in &product.elements {
                    self.allocate_type_parameter_pattern(element, parameters);
                }
            }
            TypeParameterPattern::Splice(_) => {
                unreachable!("type-parameter splices must be expanded before resolution")
            }
        }
    }

    fn scope_allocated_type_parameter_pattern(&mut self, pattern: &TypeParameterPattern) {
        match pattern {
            TypeParameterPattern::Binding(binding) => {
                let id = self.type_parameters[&binding.syntax.id];
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
            }
            TypeParameterPattern::Product(product) => {
                for element in &product.elements {
                    self.scope_allocated_type_parameter_pattern(element);
                }
            }
            TypeParameterPattern::Splice(_) => {
                unreachable!("type-parameter splices must be expanded before resolution")
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
                let outer_loop_depth = self.loop_depth;
                self.loop_depth = 0;
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
                self.loop_depth = outer_loop_depth;
                self.function_stack.pop();
                let base_name = suggested_function
                    .map(|(name, _)| name.to_owned())
                    .unwrap_or_else(|| format!("function.{}", function_id.0));
                let base_name = mangle_function_name(&base_name);
                let name = if self.multiple_modules
                    || base_name == "main"
                    || Some(self.current_module) == self.standard_library_core
                    || Some(self.current_module) == self.standard_library_cinterop
                    || Some(self.current_module) == self.standard_library_io
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
            Expression::Loop(loop_) => {
                self.loop_depth += 1;
                self.resolve_block(&loop_.body);
                self.loop_depth -= 1;
            }
            Expression::Resource(resource) => self.resolve_type(&resource.resource),
            Expression::With(with) => {
                self.resolve_type(&with.resource);
                self.resolve_expression(&with.value, Some(&with.resource), None);
                self.resolve_block(&with.body);
            }
            Expression::Block(block) => self.resolve_block(block),
            Expression::Product(product) => {
                for element in &product.elements {
                    let singleton_expected = (product.elements.len() == 1 && !element.spread)
                        .then_some(expected_type)
                        .flatten();
                    let singleton_suggestion = (product.elements.len() == 1 && !element.spread)
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
                (None, _) => {
                    let message =
                        unknown_item_message("name", &name.name, &self.private_glob_values);
                    self.diagnostics
                        .push(Diagnostic::new(name.syntax.span.clone(), message));
                }
            },
            Expression::Quote(quote) => self.diagnostics.push(Diagnostic::new(
                quote.syntax.span.clone(),
                "`quote` is only available during macro expansion",
            )),
            Expression::Splice(splice) => self.diagnostics.push(Diagnostic::new(
                splice.syntax.span.clone(),
                "splices are only available during macro expansion",
            )),
            Expression::SyntaxArgument(argument) => self.diagnostics.push(Diagnostic::new(
                argument.syntax.span.clone(),
                "grouped type or pattern syntax requires a matching macro parameter",
            )),
            Expression::VisibilityArgument(argument) => self.diagnostics.push(Diagnostic::new(
                argument.syntax.span.clone(),
                "visibility syntax requires a matching macro parameter",
            )),
            Expression::String(_)
            | Expression::CString(_)
            | Expression::Integer(_)
            | Expression::Float(_) => {}
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
                    let message =
                        unknown_item_message("type", &named.name, &self.private_glob_types);
                    self.diagnostics
                        .push(Diagnostic::new(named.syntax.span.clone(), message));
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
                for resource in &function.resources.resources {
                    self.resolve_type(resource);
                }
                self.resolve_type(&function.result);
            }
            Type::Application(application) => {
                self.resolve_type(&application.callee);
                self.resolve_type(&application.argument);
            }
            Type::Repeated(repeated) => self.resolve_type(&repeated.element),
            Type::Splice(splice) => self.diagnostics.push(Diagnostic::new(
                splice.syntax.span.clone(),
                "type splices are only available during macro expansion",
            )),
            Type::Inferred(_) | Type::StringLiteral(_) => {}
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
            let message = unknown_item_message("trait", &name.name, &self.private_glob_traits);
            self.diagnostics
                .push(Diagnostic::new(name.syntax.span.clone(), message));
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
            Pattern::Wildcard(wildcard) => self.resolve_type(&wildcard.ty),
            Pattern::StringLiteral(_) => {}
            Pattern::Binding(binding) => {
                let resolution_name = binding.resolution_name.as_deref().unwrap_or(&binding.name);
                if !binding.mutable
                    && matches!(binding.ty, Type::Inferred(_))
                    && let Some(symbol) = binding
                        .syntax
                        .definition_module()
                        .and_then(|module| {
                            self.definition_context_values
                                .get(module)
                                .and_then(|values| values.get(resolution_name))
                                .copied()
                        })
                        .or_else(|| self.lookup(resolution_name))
                    && let Some(id) = self.singleton_values.get(&symbol).copied()
                {
                    self.nominal_patterns.insert(binding.syntax.id, id);
                    if let Some(pattern_symbol) = self.declared_symbols.remove(&binding.syntax.id) {
                        self.symbols.remove(&binding.syntax.id);
                        self.symbol_modules.remove(&pattern_symbol);
                        self.symbol_declarations.remove(&pattern_symbol);
                        self.mutable_symbols.remove(&pattern_symbol);
                    }
                } else {
                    self.resolve_type(&binding.ty);
                }
            }
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
            Pattern::Splice(splice) => self.diagnostics.push(Diagnostic::new(
                splice.syntax.span.clone(),
                "pattern splices are only available during macro expansion",
            )),
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
                for resource in &function.resources.resources {
                    self.validate_public_representation(resource);
                }
                self.validate_public_representation(&function.result);
            }
            Type::Application(application) => {
                self.validate_public_representation(&application.callee);
                self.validate_public_representation(&application.argument);
            }
            Type::Repeated(repeated) => self.validate_public_representation(&repeated.element),
            Type::Inferred(_) | Type::StringLiteral(_) | Type::Splice(_) => {}
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
            Pattern::Wildcard(_) | Pattern::StringLiteral(_) | Pattern::Splice(_) => {}
            Pattern::Binding(binding) => {
                if self.nominal_patterns.contains_key(&binding.syntax.id) {
                    return;
                }
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
                if binding.mutable
                    && let Some(symbol) = self.symbols.get(&binding.syntax.id).copied()
                {
                    self.mutable_symbols.insert(symbol);
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
        if binding.mutable
            && let Some(symbol) = self.symbols.get(&binding.syntax.id).copied()
        {
            self.mutable_symbols.insert(symbol);
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
        self.symbol_modules.insert(symbol, self.current_module);
        self.symbol_declarations.insert(symbol, syntax);
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

    fn lookup_macro(&self, name: &str) -> Option<Vec<MacroId>> {
        if self.lookup(name).is_some() {
            return None;
        }
        self.declared_macros[self.current_module.0]
            .get(name)
            .cloned()
            .or_else(|| self.imported_macros.get(name).cloned())
            .or_else(|| self.prelude_macros.get(name).cloned())
    }

    fn resolve_primitive_macro(&self, callee: &Expression) -> Option<PrimitiveMacro> {
        let ids = match callee {
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
                    .cloned()
            }
            _ => None,
        }?;
        let mut primitives = ids
            .into_iter()
            .filter_map(|id| self.primitive_macros.get(&id).copied());
        let primitive = primitives.next()?;
        primitives.next().is_none().then_some(primitive)
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
                    Statement::Assignment(_)
                    | Statement::Return(_)
                    | Statement::Break(_)
                    | Statement::Continue(_)
                    | Statement::Expression(_) => {}
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
            Statement::Assignment(assignment) => {
                self.expression(&assignment.target, local, outer);
                self.expression(&assignment.value, local, outer);
            }
            Statement::Return(statement) => self.expression(&statement.value, local, outer),
            Statement::Break(statement) => {
                if let Some(value) = &statement.value {
                    self.expression(value, local, outer);
                }
            }
            Statement::Continue(_) => {}
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
            Expression::Loop(loop_) => {
                self.expression(&Expression::Block(loop_.body.clone()), local, outer);
            }
            Expression::Resource(_) => {}
            Expression::With(with) => {
                self.expression(&with.value, local, outer);
                self.expression(&Expression::Block(with.body.clone()), local, outer);
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
            Expression::SyntaxArgument(_)
            | Expression::VisibilityArgument(_)
            | Expression::Quote(_)
            | Expression::Splice(_)
            | Expression::String(_)
            | Expression::CString(_)
            | Expression::Integer(_)
            | Expression::Float(_) => {}
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
            Pattern::Wildcard(_) | Pattern::StringLiteral(_) | Pattern::Splice(_) => {}
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
