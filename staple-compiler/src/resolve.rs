use std::collections::{HashMap, HashSet};

use crate::{ModuleId, Program};
use staple_syntax::{
    Accessor, Binding, BindingKind, BlockExpression, Diagnostic, Expression, Item,
    MacroDeclaration, Module, Pattern, PatternBindingKind, Span, Submodule, SyntaxId, Type,
    TypeDeclaration, TypeParameterPattern, UseDeclaration, UseKind, Visibility,
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
    CompileTime(SyntaxId),
}

#[derive(Debug, Clone)]
pub struct ResolvedTrait {
    pub id: TraitId,
    pub declaration: staple_syntax::TraitDeclaration,
    pub parameters: Vec<TypeParameterId>,
    pub functional_dependencies: Vec<ResolvedFunctionalDependency>,
    pub methods: Vec<TraitMethodId>,
    pub default_methods: HashMap<TraitMethodId, FunctionId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFunctionalDependency {
    pub determinants: Vec<TypeParameterId>,
    pub dependent: TypeParameterId,
}

#[derive(Debug, Clone)]
pub struct ResolvedTraitImplementation {
    pub syntax: SyntaxId,
    pub trait_id: TraitId,
    pub parameters: Vec<TypeParameterId>,
    pub arguments: Vec<Type>,
    pub trait_bounds: Vec<staple_syntax::TraitBound>,
    pub subtype_bounds: Vec<staple_syntax::SubtypeBound>,
    pub negative: bool,
    pub methods: HashMap<TraitMethodId, FunctionId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltinType {
    Integer(IntegerType),
    Float(FloatType),
    String,
    Ref,
    Slice,
    Buffer,
    CChar,
    CString,
    CPointer,
    IO,
    Reactive,
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
    Slice,
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
    pub docs: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompileTimeBindingKind {
    MacroParameter,
    Helper,
    HelperParameter,
    Local,
    Builtin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileTimeBindingInfo {
    pub declaration: SyntaxId,
    pub name: String,
    pub type_display: Option<String>,
    pub kind: CompileTimeBindingKind,
    pub mutable: bool,
    pub declaration_prefix: Option<String>,
    pub module: ModuleId,
    pub definition: Option<DefinitionId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntrinsicFunction {
    ToString {
        value: NumericType,
    },
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
    StringAdd,
    SliceLength,
    SliceFromRef,
    BufferWithCapacity,
    BufferLength,
    BufferCapacity,
    BufferPush,
    BufferPop,
    BufferGet,
    BufferFreeze,
    BufferTransfer,
    BufferClone,
    RefReplace,
    Drop,
    ReactiveScope,
    Reaction,
    Batch,
    Snapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NumericType {
    Integer(IntegerType),
    Float(FloatType),
}

#[derive(Debug, Clone)]
pub struct ResolvedFunction {
    pub id: FunctionId,
    pub name: String,
    pub parameter_style: staple_syntax::FunctionParameterStyle,
    pub binding_syntax: Option<SyntaxId>,
    pub pattern: Pattern,
    pub result_annotation: Option<Type>,
    pub binding_annotation: Option<Type>,
    pub type_parameters: Vec<TypeParameterPattern>,
    pub trait_bounds: Vec<staple_syntax::TraitBound>,
    pub subtype_bounds: Vec<staple_syntax::SubtypeBound>,
    pub captures: Vec<SymbolId>,
    pub body: Expression,
}

#[derive(Debug, Clone)]
pub struct ResolvedModule {
    program: Program,
    functions: Vec<ResolvedFunction>,
    symbols: HashMap<SyntaxId, SymbolId>,
    namespace_references: HashMap<SyntaxId, ModuleId>,
    function_expressions: HashMap<SyntaxId, FunctionId>,
    named_types: HashMap<SyntaxId, TypeId>,
    nominal_patterns: HashMap<SyntaxId, TypeId>,
    type_parameters: HashMap<SyntaxId, TypeParameterId>,
    type_parameter_sized: HashMap<TypeParameterId, bool>,
    effect_parameters: HashSet<TypeParameterId>,
    type_parameter_declarations: HashMap<TypeParameterId, SyntaxId>,
    type_parameter_modules: HashMap<TypeParameterId, ModuleId>,
    type_declarations: HashMap<TypeId, TypeDeclaration>,
    type_names: HashMap<TypeId, String>,
    syntax_modules: HashMap<SyntaxId, ModuleId>,
    builtin_types: HashMap<TypeId, BuiltinType>,
    recursive_constructions: HashMap<TypeId, RecursiveConstruction>,
    intrinsic_functions: HashMap<SymbolId, IntrinsicFunction>,
    external_symbols: HashSet<SymbolId>,
    macro_calls: HashMap<SyntaxId, PrimitiveMacro>,
    macro_definitions: HashMap<SyntaxId, ResolvedMacro>,
    macro_invocations: HashMap<SyntaxId, ResolvedMacro>,
    macro_declarations: HashMap<MacroId, SyntaxId>,
    macro_modules: HashMap<MacroId, ModuleId>,
    quote_macros: HashMap<SyntaxId, MacroId>,
    constructors: HashMap<SymbolId, TypeId>,
    singleton_values: HashMap<SymbolId, TypeId>,
    type_modules: HashMap<TypeId, ModuleId>,
    traits: HashMap<TraitId, ResolvedTrait>,
    trait_methods: HashMap<TraitMethodId, staple_syntax::TraitMember>,
    trait_method_traits: HashMap<TraitMethodId, TraitId>,
    trait_references: HashMap<SyntaxId, TraitId>,
    trait_method_references: HashMap<SyntaxId, Vec<TraitMethodId>>,
    trait_implementations: Vec<ResolvedTraitImplementation>,
    standard_traits: HashMap<String, TraitId>,
    checked_initialization_symbols: HashSet<SymbolId>,
    checked_initialization_reads: HashSet<SyntaxId>,
    mutable_symbols: HashSet<SymbolId>,
    signal_symbols: HashSet<SymbolId>,
    const_symbols: HashSet<SymbolId>,
    /// Symbols whose binding carries a `mut` marker in source, regardless of
    /// whether the resolver later treats that marker as ordinary mutable
    /// local storage. A `mut` function parameter is stripped from
    /// `mutable_symbols` (see the comment at its removal site) because it
    /// denotes a by-address write effect, not an ordinary mutable local -
    /// but it should still read and highlight as `mut` everywhere it's
    /// named, so this set is never pruned the way `mutable_symbols` is.
    mutable_annotations: HashSet<SymbolId>,
    symbol_owners: HashMap<SymbolId, Option<FunctionId>>,
    symbol_modules: HashMap<SymbolId, ModuleId>,
    symbol_declarations: HashMap<SymbolId, SyntaxId>,
    trait_modules: HashMap<TraitId, ModuleId>,
    import_definitions: HashMap<(SyntaxId, String), Vec<DefinitionId>>,
    visible_module_definitions: Vec<HashMap<String, Vec<DefinitionId>>>,
    exported_module_definitions: Vec<HashMap<String, Vec<DefinitionId>>>,
    compile_time_bindings: HashMap<SyntaxId, CompileTimeBindingInfo>,
    companion_members: HashMap<TypeId, HashMap<String, ResolvedCompanionMember>>,
    companion_type_for_module: HashMap<ModuleId, TypeId>,
}

#[derive(Debug, Clone, Copy)]
struct ResolvedCompanionMember {
    symbol: SymbolId,
    declaring_module: ModuleId,
    public: bool,
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

    /// The module a segment of a qualified access chain refers to, e.g. the
    /// `std`/`io` segments of `std.io.println` or the `List` segment of
    /// `List.push` (where `List`'s companion block is registered as a
    /// module). Populated only for segments that are part of a chain that
    /// resolved as a qualified path, not for ordinary field access.
    pub fn namespace_for(&self, syntax_id: SyntaxId) -> Option<ModuleId> {
        self.namespace_references.get(&syntax_id).copied()
    }

    /// The type whose `companion { ... }` block a given module is, if any.
    pub fn companion_type_for_module(&self, module: ModuleId) -> Option<TypeId> {
        self.companion_type_for_module.get(&module).copied()
    }

    pub fn macro_definition_for(&self, syntax_id: SyntaxId) -> Option<&ResolvedMacro> {
        self.macro_definitions.get(&syntax_id)
    }

    pub fn macro_invocation_for(&self, syntax_id: SyntaxId) -> Option<&ResolvedMacro> {
        self.macro_invocations.get(&syntax_id)
    }

    pub fn compile_time_binding_for(&self, syntax_id: SyntaxId) -> Option<&CompileTimeBindingInfo> {
        self.compile_time_bindings.get(&syntax_id)
    }

    pub fn companion_member(
        &self,
        ty: TypeId,
        name: &str,
        accessing_module: Option<ModuleId>,
    ) -> Option<SymbolId> {
        let member = self.companion_members.get(&ty)?.get(name)?;
        (member.public || accessing_module == Some(member.declaring_module))
            .then_some(member.symbol)
    }

    pub fn companion_members(
        &self,
        ty: TypeId,
        accessing_module: Option<ModuleId>,
    ) -> Vec<(&str, SymbolId)> {
        self.companion_members
            .get(&ty)
            .into_iter()
            .flat_map(|members| members.iter())
            .filter_map(|(name, member)| {
                (member.public || accessing_module == Some(member.declaring_module))
                    .then_some((name.as_str(), member.symbol))
            })
            .collect()
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
        if let Some(info) = self.compile_time_binding_for(syntax_id) {
            definitions.push(
                info.definition
                    .unwrap_or(DefinitionId::CompileTime(info.declaration)),
            );
        }
        if let Some(id) = self.quote_macros.get(&syntax_id) {
            definitions.push(DefinitionId::Macro(*id));
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

    pub fn exported_definitions(
        &self,
        module: ModuleId,
    ) -> Option<&HashMap<String, Vec<DefinitionId>>> {
        self.exported_module_definitions.get(module.0)
    }

    pub fn trait_methods(&self, trait_id: TraitId) -> Vec<TraitMethodId> {
        self.trait_method_traits
            .iter()
            .filter_map(|(method, owner)| (*owner == trait_id).then_some(*method))
            .collect()
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
            DefinitionId::CompileTime(syntax) => Some(syntax),
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
            DefinitionId::CompileTime(syntax) => self
                .compile_time_bindings
                .get(&syntax)
                .map(|info| info.module),
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
        let declaration = &self.type_declarations[&id];
        let visibility = if declaration.kind == staple_syntax::TypeDeclarationKind::Singleton {
            declaration.visibility
        } else {
            declaration.representation_visibility
        };
        if visibility == Visibility::Public {
            return true;
        }
        let Some(defining_module) = self.type_modules.get(&id).copied() else {
            return false;
        };
        if visibility == Visibility::Package
            && self
                .program
                .package_of(defining_module)
                .zip(self.program.package_of(module))
                .is_some_and(|(left, right)| left == right)
        {
            return true;
        }
        let mut current = Some(module);
        while let Some(candidate) = current {
            if candidate == defining_module {
                return true;
            }
            current = self.program.parent_module(candidate);
        }
        false
    }

    pub fn type_parameter_for(&self, syntax_id: SyntaxId) -> Option<TypeParameterId> {
        self.type_parameters.get(&syntax_id).copied()
    }

    pub fn type_parameter_is_sized(&self, id: TypeParameterId) -> bool {
        self.type_parameter_sized.get(&id).copied().unwrap_or(true)
    }
    pub fn is_effect_parameter(&self, id: TypeParameterId) -> bool {
        self.effect_parameters.contains(&id)
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

    pub fn trait_method(&self, id: TraitMethodId) -> Option<&staple_syntax::TraitMember> {
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

    pub fn is_signal_symbol(&self, symbol: SymbolId) -> bool {
        self.signal_symbols.contains(&symbol)
    }

    pub fn is_const_symbol(&self, symbol: SymbolId) -> bool {
        self.const_symbols.contains(&symbol)
    }

    /// Whether `symbol`'s binding carries a `mut` marker in source. Unlike
    /// [`is_mutable_symbol`](Self::is_mutable_symbol), this stays true for a
    /// `mut` function parameter even though that marker denotes a by-address
    /// write effect rather than ordinary mutable local storage - it's meant
    /// for callers that only care how the binding reads in source (e.g.
    /// hover text, highlighting), not how it's compiled.
    pub fn has_mutable_annotation(&self, symbol: SymbolId) -> bool {
        self.mutable_annotations.contains(&symbol)
    }

    pub fn is_module_symbol(&self, symbol: SymbolId) -> bool {
        self.symbol_owners.get(&symbol) == Some(&None)
    }

    pub fn is_top_level_symbol(&self, symbol: SymbolId) -> bool {
        fn pattern_contains(module: &ResolvedModule, pattern: &Pattern, symbol: SymbolId) -> bool {
            match pattern {
                Pattern::Binding(value) => module.symbol_for(value.syntax.id) == Some(symbol),
                Pattern::At(value) => {
                    module.symbol_for(value.binding.syntax.id) == Some(symbol)
                        || pattern_contains(module, &value.pattern, symbol)
                }
                Pattern::Product(value) => value
                    .elements
                    .iter()
                    .any(|element| pattern_contains(module, element, symbol)),
                Pattern::Nominal(value) => pattern_contains(module, &value.argument, symbol),
                Pattern::Wildcard(_) | Pattern::StringLiteral(_) | Pattern::Splice(_) => false,
            }
        }
        self.program.modules().iter().any(|source| {
            source.syntax.items.iter().any(|item| match item {
                Item::Binding(value) => self.symbol_for(value.syntax.id) == Some(symbol),
                Item::PatternBinding(value) => pattern_contains(self, &value.pattern, symbol),
                _ => false,
            })
        })
    }

    pub(crate) fn symbol_owner(&self, symbol: SymbolId) -> Option<FunctionId> {
        self.symbol_owners.get(&symbol).copied().flatten()
    }

    pub(crate) fn function_for_symbol(&self, symbol: SymbolId) -> Option<FunctionId> {
        self.functions.iter().find_map(|function| {
            function
                .binding_syntax
                .and_then(|syntax| (self.symbol_for(syntax) == Some(symbol)).then_some(function.id))
        })
    }

    pub fn has_mutable_storage(&self, symbol: SymbolId) -> bool {
        self.is_mutable_symbol(symbol) || self.is_signal_symbol(symbol)
    }

    pub fn symbol_module(&self, symbol: SymbolId) -> Option<ModuleId> {
        self.symbol_modules.get(&symbol).copied()
    }
}

#[derive(Clone, Default)]
struct Interface {
    values: HashMap<String, SymbolId>,
    types: HashMap<String, TypeId>,
    macros: HashMap<String, Vec<MacroId>>,
    traits: HashMap<String, TraitId>,
    namespaces: HashMap<String, ModuleId>,
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

fn item_uses_package_visibility(item: &Item) -> bool {
    let is_package = |visibility| visibility == Visibility::Package;
    match item {
        Item::ExternBlock(item) => is_package(item.visibility),
        Item::TypeDeclaration(item) => {
            is_package(item.visibility) || is_package(item.representation_visibility)
        }
        Item::MacroDeclaration(item) => is_package(item.visibility),
        Item::TraitDeclaration(item) => is_package(item.visibility),
        Item::Binding(item) => is_package(item.visibility),
        Item::Submodule(item) => is_package(item.visibility),
        Item::UseDeclaration(item) => is_package(item.visibility),
        Item::Modified(item) => item_uses_package_visibility(&item.item),
        Item::VisibilitySplice(item) => item_uses_package_visibility(&item.item),
        Item::VisibilityMacroInvocation(item) => matches!(
            item.visibility.kind,
            staple_syntax::VisibilityKind::Package | staple_syntax::VisibilityKind::PublicReprPackage
        ),
        _ => false,
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
    for name in imported.namespaces.keys() {
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
    if let Some(module) = imported.namespaces.get(item) {
        changed |= exported
            .namespaces
            .insert(alias.to_owned(), *module)
            .is_none();
    }
    changed
}

fn reexport_declaration(
    program: &Program,
    declaration: &UseDeclaration,
    interfaces: &[Interface],
    exported: &mut Interface,
) -> bool {
    let Some(imported_module) = program.imported_module(declaration.syntax.id) else {
        return false;
    };
    let imported = &interfaces[imported_module.0];
    match &declaration.kind {
        UseKind::Dotted => {
            let Some(candidates) = program.dotted_import(declaration.syntax.id) else {
                return false;
            };
            let Some(item_module) = candidates.item_module else {
                return false;
            };
            let item = declaration
                .path
                .last()
                .expect("dotted import has a final component");
            let imported = &interfaces[item_module.0];
            if let Some(namespace) = candidates.namespace {
                let different_namespace = imported
                    .namespaces
                    .get(item)
                    .is_some_and(|module| *module != namespace);
                let compatible_item =
                    imported.types.contains_key(item) || imported.traits.contains_key(item);
                let conflicts = imported.values.contains_key(item)
                    || imported.macros.contains_key(item)
                    || different_namespace;
                if compatible_item && !conflicts {
                    return export_interface_item(exported, imported, item, item)
                        | exported
                            .namespaces
                            .insert(item.clone(), namespace)
                            .is_none();
                }
                false
            } else {
                export_interface_item(exported, imported, item, item)
            }
        }
        UseKind::Namespace => false,
        UseKind::Glob => extend_interface(exported, imported),
        UseKind::Selected(names) => names.iter().fold(false, |changed, import| {
            export_interface_item(exported, imported, &import.name, import.alias()) | changed
        }),
        UseKind::Renamed { item, alias } => export_interface_item(exported, imported, item, alias),
    }
}

#[derive(Default)]
pub struct NameResolver {
    scopes: Vec<HashMap<String, SymbolId>>,
    shadowable: Vec<HashMap<String, bool>>,
    namespaces: Vec<HashMap<String, ModuleId>>,
    submodule_ids: HashMap<SyntaxId, ModuleId>,
    block_types: Vec<HashMap<String, TypeId>>,
    type_ids_by_syntax: HashMap<SyntaxId, TypeId>,
    imported_module_ids: HashMap<SyntaxId, ModuleId>,
    resolved_use_kinds: HashMap<SyntaxId, UseKind>,
    additional_imported_namespaces: HashMap<SyntaxId, ModuleId>,
    module_parents: HashMap<ModuleId, ModuleId>,
    imported_types: Vec<HashMap<String, TypeId>>,
    imported_macros: Vec<HashMap<String, Vec<MacroId>>>,
    imported_traits: Vec<HashMap<String, TraitId>>,
    private_glob_values: HashMap<String, Vec<String>>,
    private_glob_types: HashMap<String, Vec<String>>,
    private_glob_traits: HashMap<String, Vec<String>>,
    visible_trait_methods: HashMap<String, Vec<TraitMethodId>>,
    type_parameter_scopes: Vec<HashMap<String, TypeParameterId>>,
    prelude_values: HashMap<String, SymbolId>,
    prelude_types: HashMap<String, TypeId>,
    prelude_macros: HashMap<String, Vec<MacroId>>,
    prelude_traits: HashMap<String, TraitId>,
    prelude_namespaces: HashMap<String, ModuleId>,
    symbols: HashMap<SyntaxId, SymbolId>,
    namespace_references: HashMap<SyntaxId, ModuleId>,
    function_expressions: HashMap<SyntaxId, FunctionId>,
    symbol_owners: HashMap<SymbolId, Option<FunctionId>>,
    function_parents: HashMap<FunctionId, Option<FunctionId>>,
    function_captures: HashMap<FunctionId, Vec<SymbolId>>,
    function_stack: Vec<FunctionId>,
    loop_depth: usize,
    builtin_types: HashMap<TypeId, BuiltinType>,
    recursive_constructions: HashMap<TypeId, RecursiveConstruction>,
    intrinsic_functions: HashMap<SymbolId, IntrinsicFunction>,
    primitive_macros: HashMap<MacroId, PrimitiveMacro>,
    macro_calls: HashMap<SyntaxId, PrimitiveMacro>,
    macro_declarations: HashMap<MacroId, SyntaxId>,
    macro_modules: HashMap<MacroId, ModuleId>,
    quote_macros: HashMap<SyntaxId, MacroId>,
    constructors: HashMap<SymbolId, TypeId>,
    singleton_values: HashMap<SymbolId, TypeId>,
    nominal_patterns: HashMap<SyntaxId, TypeId>,
    type_modules: HashMap<TypeId, ModuleId>,
    type_constructor_symbols: HashMap<TypeId, SymbolId>,
    functions: Vec<ResolvedFunction>,
    named_types: HashMap<SyntaxId, TypeId>,
    type_parameters: HashMap<SyntaxId, TypeParameterId>,
    type_parameter_sized: HashMap<TypeParameterId, bool>,
    effect_parameters: HashSet<TypeParameterId>,
    type_parameter_declarations: HashMap<TypeParameterId, SyntaxId>,
    type_parameter_modules: HashMap<TypeParameterId, ModuleId>,
    type_declarations: HashMap<TypeId, TypeDeclaration>,
    type_names: HashMap<TypeId, String>,
    traits: HashMap<TraitId, ResolvedTrait>,
    trait_methods: HashMap<TraitMethodId, staple_syntax::TraitMember>,
    trait_method_traits: HashMap<TraitMethodId, TraitId>,
    trait_member_ids: HashMap<(TraitId, String), TraitMethodId>,
    trait_modules: HashMap<TraitId, ModuleId>,
    trait_references: HashMap<SyntaxId, TraitId>,
    trait_method_references: HashMap<SyntaxId, Vec<TraitMethodId>>,
    trait_implementations: Vec<ResolvedTraitImplementation>,
    syntax_modules: HashMap<SyntaxId, ModuleId>,
    mutable_symbols: HashSet<SymbolId>,
    signal_symbols: HashSet<SymbolId>,
    const_symbols: HashSet<SymbolId>,
    mutable_annotations: HashSet<SymbolId>,
    symbol_modules: HashMap<SymbolId, ModuleId>,
    import_definitions: HashMap<(SyntaxId, String), Vec<DefinitionId>>,
    visible_module_definitions: Vec<HashMap<String, Vec<DefinitionId>>>,
    interfaces: Vec<Interface>,
    package_interfaces: Vec<Interface>,
    module_packages: Vec<Option<staple_project::PackageId>>,
    declared_symbols: HashMap<SyntaxId, SymbolId>,
    symbol_declarations: HashMap<SymbolId, SyntaxId>,
    module_values: Vec<HashMap<String, SymbolId>>,
    definition_context_values: Vec<HashMap<String, SymbolId>>,
    definition_context_types: Vec<HashMap<String, TypeId>>,
    definition_context_namespaces: Vec<HashMap<String, ModuleId>>,
    binding_type_parameters: HashMap<SyntaxId, Vec<TypeParameterPattern>>,
    binding_trait_bounds: HashMap<SyntaxId, Vec<staple_syntax::TraitBound>>,
    binding_subtype_bounds: HashMap<SyntaxId, Vec<staple_syntax::SubtypeBound>>,
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
    standard_library_prelude: Option<ModuleId>,
    standard_library_syntax: Option<ModuleId>,
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

    /// Resolves on a dedicated thread with a generous stack size: this is a
    /// deep recursive-descent walk over the whole program (standard library
    /// included), and some callers (notably test harnesses) run on threads
    /// with a smaller default stack than a typical `main` thread provides.
    pub fn resolve_program(self, program: Program) -> Result<ResolvedModule, Vec<Diagnostic>> {
        std::thread::Builder::new()
            .stack_size(256 * 1024 * 1024)
            .spawn(move || self.resolve_program_inner(program))
            .expect("name resolution thread should spawn")
            .join()
            .expect("name resolution should not panic")
    }

    fn resolve_program_inner(
        mut self,
        program: Program,
    ) -> Result<ResolvedModule, Vec<Diagnostic>> {
        let (mut program, mut macro_analysis) = crate::macro_expand::expand_program(program)?;
        let mut next_syntax_id = macro_analysis.next_syntax_id;
        crate::macro_expand::desugar_program(&mut program, &mut next_syntax_id);
        crate::macro_expand::desugar_macro_analysis(&mut macro_analysis, &mut next_syntax_id);
        self.standard_library_core = program.standard_library_core();
        self.standard_library_prelude = program.standard_library_prelude();
        self.standard_library_syntax = program.standard_library_syntax();
        self.standard_library_cinterop = program.standard_library_cinterop();
        self.standard_library_io = program.standard_library_io();
        self.module_packages = program
            .modules()
            .iter()
            .map(|module| program.package_of(module.id))
            .collect();
        for module in program.modules() {
            for item in &module.syntax.items {
                if item_uses_package_visibility(item) && program.package_of(module.id).is_none() {
                    self.diagnostics.push(Diagnostic::new(
                        item.syntax().span.clone(),
                        "package visibility requires a Binder manifest",
                    ));
                }
            }
        }
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
        self.next_syntax_id = next_syntax_id;
        self.collect_interfaces(&program);
        for span in program.resolve_dotted_imports(|module, name, namespace| {
            let interface = &self.interfaces[module.0];
            let different_namespace = interface
                .namespaces
                .get(name)
                .is_some_and(|item_module| Some(*item_module) != namespace);
            let any_item = interface.values.contains_key(name)
                || interface.types.contains_key(name)
                || interface.macros.contains_key(name)
                || interface.traits.contains_key(name)
                || different_namespace;
            let conflicts = interface.values.contains_key(name)
                || interface.macros.contains_key(name)
                || different_namespace;
            (any_item, conflicts)
        }) {
            self.diagnostics.push(Diagnostic::new(
                span,
                "ambiguous dotted import: the final component is both a module and a public item",
            ));
        }
        self.submodule_ids = program.child_modules().clone();
        self.imported_module_ids = program.imported_modules().clone();
        self.resolved_use_kinds = program.resolved_use_kinds().clone();
        self.additional_imported_namespaces = program.additional_imported_namespaces().clone();
        self.module_parents = program
            .modules()
            .iter()
            .filter_map(|module| module.parent.map(|parent| (module.id, parent)))
            .collect();
        self.visible_module_definitions = vec![HashMap::new(); program.modules().len()];
        self.build_definition_context_values(&program);
        self.collect_standard_library_contract(&program);
        for source_module in program.modules() {
            self.current_module = source_module.id;
            self.scopes.clear();
            self.shadowable.clear();
            self.namespaces.clear();
            self.block_types.clear();
            self.imported_types.clear();
            self.imported_macros.clear();
            self.imported_traits.clear();
            self.private_glob_values.clear();
            self.private_glob_types.clear();
            self.private_glob_traits.clear();
            self.visible_trait_methods.clear();
            self.prelude_values.clear();
            self.prelude_types.clear();
            self.prelude_macros.clear();
            self.prelude_traits.clear();
            self.prelude_namespaces.clear();
            self.push_scope();
            self.namespaces.push(HashMap::new());
            self.imported_types.push(HashMap::new());
            self.imported_macros.push(HashMap::new());
            self.imported_traits.push(HashMap::new());
            self.install_root_namespaces(&program, source_module.id);
            self.install_child_namespaces(&program, source_module.id);
            self.install_prelude(&program, source_module.id);
            self.install_imports(&program, source_module.id);
            self.install_local_traits(source_module.id);
            if source_module.companion
                && let Some(parent) = source_module.parent
            {
                self.scopes
                    .last_mut()
                    .expect("resolver value scope")
                    .extend(self.module_values[parent.0].clone());
                self.imported_types
                    .last_mut()
                    .expect("resolver type scope")
                    .extend(self.declared_types[parent.0].clone());
                let own_namespaces = self.namespaces.pop().expect("resolver namespace scope");
                let mut namespaces = self.definition_context_namespaces[parent.0].clone();
                namespaces.extend(own_namespaces);
                self.namespaces.push(namespaces);
            }
            self.predeclare_items(&source_module.syntax.items);
            self.record_visible_module_definitions(source_module.id);
            for item in &source_module.syntax.items {
                self.resolve_item(item);
            }
            for (_, helper) in macro_analysis
                .helpers
                .iter()
                .filter(|(module, _)| *module == source_module.id)
            {
                self.resolve_compile_time_helper_annotations(helper);
            }
            self.imported_traits.pop();
            self.imported_macros.pop();
            self.imported_types.pop();
            self.namespaces.pop();
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
        let mut standard_traits = self
            .standard_library_core
            .map(|core| self.interfaces[core.0].traits.clone())
            .unwrap_or_default();
        if let Some(prelude) = self.standard_library_prelude {
            standard_traits.extend(self.interfaces[prelude.0].traits.clone());
        }
        if let Some(syntax) = self.standard_library_syntax {
            standard_traits.extend(self.interfaces[syntax.0].traits.clone());
        }
        let mut compile_time_bindings =
            analyze_compile_time_bindings(&program, &macro_analysis.helpers);
        for info in compile_time_bindings.values_mut() {
            if info.kind == CompileTimeBindingKind::Builtin {
                info.definition = self
                    .definition_context_types
                    .get(info.module.0)
                    .and_then(|types| types.get(&info.name))
                    .copied()
                    .map(DefinitionId::Type);
            }
        }
        let mut companion_type_for_module = HashMap::new();
        let companion_members = self
            .type_modules
            .iter()
            .filter_map(|(ty, owner)| {
                let name = &self.type_declarations[ty].name;
                let child = program.child_named(*owner, name)?;
                program.module(child).companion.then(|| {
                    companion_type_for_module.insert(child, *ty);
                    let members = self.module_values[child.0]
                        .iter()
                        .map(|(name, symbol)| {
                            (
                                name.clone(),
                                ResolvedCompanionMember {
                                    symbol: *symbol,
                                    declaring_module: *owner,
                                    public: self.interfaces[child.0].values.contains_key(name),
                                },
                            )
                        })
                        .collect();
                    (*ty, members)
                })
            })
            .collect();
        let exported_module_definitions = self
            .interfaces
            .iter()
            .map(|interface| {
                let mut definitions = HashMap::<String, Vec<DefinitionId>>::new();
                for (name, symbol) in &interface.values {
                    definitions
                        .entry(name.clone())
                        .or_default()
                        .push(DefinitionId::Symbol(*symbol));
                }
                for (name, ty) in &interface.types {
                    definitions
                        .entry(name.clone())
                        .or_default()
                        .push(DefinitionId::Type(*ty));
                }
                for (name, trait_id) in &interface.traits {
                    definitions
                        .entry(name.clone())
                        .or_default()
                        .push(DefinitionId::Trait(*trait_id));
                }
                for (name, macros) in &interface.macros {
                    definitions
                        .entry(name.clone())
                        .or_default()
                        .extend(macros.iter().copied().map(DefinitionId::Macro));
                }
                for (name, module) in &interface.namespaces {
                    definitions
                        .entry(name.clone())
                        .or_default()
                        .push(DefinitionId::Module(*module));
                }
                definitions
            })
            .collect();
        let mut resolved = ResolvedModule {
            program,
            functions: self.functions,
            symbols: self.symbols,
            namespace_references: self.namespace_references,
            function_expressions: self.function_expressions,
            named_types: self.named_types,
            nominal_patterns: self.nominal_patterns,
            type_parameters: self.type_parameters,
            type_parameter_sized: self.type_parameter_sized,
            effect_parameters: self.effect_parameters,
            type_parameter_declarations: self.type_parameter_declarations,
            type_parameter_modules: self.type_parameter_modules,
            type_declarations: self.type_declarations,
            type_names: self.type_names,
            syntax_modules: self.syntax_modules,
            builtin_types: self.builtin_types,
            recursive_constructions: self.recursive_constructions,
            intrinsic_functions: self.intrinsic_functions,
            external_symbols,
            macro_calls: self.macro_calls,
            macro_definitions: macro_analysis.definitions,
            macro_invocations: macro_analysis.invocations,
            macro_declarations: self.macro_declarations,
            macro_modules: self.macro_modules,
            quote_macros: self.quote_macros,
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
            signal_symbols: self.signal_symbols,
            const_symbols: self.const_symbols,
            mutable_annotations: self.mutable_annotations,
            symbol_owners: self.symbol_owners,
            symbol_modules: self.symbol_modules,
            symbol_declarations: self.symbol_declarations,
            import_definitions: self.import_definitions,
            visible_module_definitions: self.visible_module_definitions,
            exported_module_definitions,
            compile_time_bindings,
            companion_members,
            companion_type_for_module,
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
        self.register_builtin_type(core, "std.core", "Ref", BuiltinType::Ref);
        for (path, namespace, name, builtin) in [
            (
                "std/string.sta",
                "std.string",
                "String",
                BuiltinType::String,
            ),
            ("std/slice.sta", "std.slice", "Slice", BuiltinType::Slice),
            (
                "std/buffer.sta",
                "std.buffer",
                "Buffer",
                BuiltinType::Buffer,
            ),
        ] {
            if let Some(module) = program
                .modules()
                .iter()
                .find(|module| module.path.ends_with(path))
                .map(|module| module.id)
            {
                self.register_builtin_type(module, namespace, name, builtin);
            }
        }
        let Some(syntax) = program.standard_library_syntax() else {
            return;
        };
        for name in [
            "Ident",
            "CallExpr",
            "StringExpr",
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
            "BindingPattern",
            "NominalPattern",
            "AliasDeclaration",
            "DistinctDeclaration",
            "SingletonDeclaration",
            "OpaqueDeclaration",
            "TypeDeclarationKind",
            "TypeDeclarationItem",
            "Modifier",
            "ModifiedItem",
            "UnstructuredItem",
            "Item",
            "Syntax",
            "SyntaxNode",
        ] {
            self.register_builtin_type(syntax, "std.syntax", name, BuiltinType::Syntax);
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
        if let Some(reactive) = program
            .modules()
            .iter()
            .find(|module| module.path.ends_with("std/core/reactive.sta"))
            .map(|module| module.id)
        {
            self.register_builtin_type(
                reactive,
                "std.core.reactive",
                "Reactive",
                BuiltinType::Reactive,
            );
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
            expected.push((
                format!("__{}_to_string", integer.intrinsic_name()),
                IntrinsicFunction::ToString {
                    value: NumericType::Integer(integer),
                },
            ));
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
            expected.push((
                format!("__{}_to_string", float.intrinsic_name()),
                IntrinsicFunction::ToString {
                    value: NumericType::Float(float),
                },
            ));
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
        expected.push(("__string_add".to_owned(), IntrinsicFunction::StringAdd));
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
        if let Some(symbol) = self.interfaces[core.0]
            .namespaces
            .get("Ref")
            .and_then(|companion| self.interfaces[companion.0].values.get("replace"))
            .copied()
        {
            self.intrinsic_functions
                .insert(symbol, IntrinsicFunction::RefReplace);
        }
        let slice_module = program
            .modules()
            .iter()
            .find(|module| module.path.ends_with("std/slice.sta"))
            .map(|module| module.id);
        if let Some(symbol) = slice_module.and_then(|slice| {
            self.interfaces[slice.0]
                .namespaces
                .get("Slice")
                .and_then(|companion| self.interfaces[companion.0].values.get("length"))
                .copied()
        }) {
            self.intrinsic_functions
                .insert(symbol, IntrinsicFunction::SliceLength);
        }
        if let Some(symbol) = slice_module.and_then(|slice| {
            self.interfaces[slice.0]
                .namespaces
                .get("Slice")
                .and_then(|companion| self.interfaces[companion.0].values.get("from_ref"))
                .copied()
        }) {
            self.intrinsic_functions
                .insert(symbol, IntrinsicFunction::SliceFromRef);
        }
        for (name, _) in expected {
            if !found.contains_key(&name) {
                self.diagnostics.push(Diagnostic::new(
                    Span::Compiler,
                    format!("standard library `std.core` does not declare intrinsic `{name}`"),
                ));
            }
        }
        for (name, intrinsic) in [
            (
                "__buffer_with_capacity",
                IntrinsicFunction::BufferWithCapacity,
            ),
            ("__buffer_length", IntrinsicFunction::BufferLength),
            ("__buffer_capacity", IntrinsicFunction::BufferCapacity),
            ("__buffer_push", IntrinsicFunction::BufferPush),
            ("__buffer_pop", IntrinsicFunction::BufferPop),
            ("__buffer_get", IntrinsicFunction::BufferGet),
            ("__buffer_freeze", IntrinsicFunction::BufferFreeze),
            ("__buffer_transfer", IntrinsicFunction::BufferTransfer),
            ("__buffer_clone", IntrinsicFunction::BufferClone),
            ("__reactive_scope", IntrinsicFunction::ReactiveScope),
            ("reaction", IntrinsicFunction::Reaction),
            ("batch", IntrinsicFunction::Batch),
            ("snapshot", IntrinsicFunction::Snapshot),
        ] {
            let symbol = program
                .modules()
                .iter()
                .filter(|module| module.path.starts_with(standard_library_directory))
                .find_map(|module| self.module_values[module.id.0].get(name).copied());
            if let Some(symbol) = symbol {
                self.intrinsic_functions.insert(symbol, intrinsic);
            } else {
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
                    let exported = &mut self.interfaces[source_module.id.0];
                    changed |= reexport_declaration(program, declaration, &previous, exported);
                }
            }
            if !changed {
                break;
            }
        }
        for module in program.modules() {
            extend_interface(
                &mut self.package_interfaces[module.id.0],
                &self.interfaces[module.id.0],
            );
        }
        loop {
            let previous = self.package_interfaces.clone();
            let mut changed = false;
            for source_module in program.modules() {
                for item in &source_module.syntax.items {
                    let Item::UseDeclaration(declaration) = item else {
                        continue;
                    };
                    if declaration.visibility != Visibility::Package {
                        continue;
                    }
                    let Some(imported) = program.imported_module(declaration.syntax.id) else {
                        continue;
                    };
                    let visible = if self.same_package(source_module.id, imported) {
                        &previous
                    } else {
                        &self.interfaces
                    };
                    changed |= reexport_declaration(
                        program,
                        declaration,
                        visible,
                        &mut self.package_interfaces[source_module.id.0],
                    );
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
            if declaration.kind != staple_syntax::TypeDeclarationKind::Distinct {
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
                declaration.kind == staple_syntax::TypeDeclarationKind::Distinct
                    && declaration.representation_visibility == Visibility::Public
            } else if builtin == BuiltinType::Slice {
                declaration.kind == staple_syntax::TypeDeclarationKind::Distinct
                    && declaration.representation_visibility == Visibility::Private
            } else {
                declaration.kind == staple_syntax::TypeDeclarationKind::Opaque
            };
            if !valid_kind {
                self.diagnostics.push(Diagnostic::new(
                    declaration.syntax.span.clone(),
                    format!("standard library type `{name}` has an invalid representation"),
                ));
            }
        }
        if matches!(
            builtin,
            BuiltinType::CPointer | BuiltinType::Ref | BuiltinType::Slice | BuiltinType::Buffer
        ) && declaration.type_parameters.len() != 1
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
            BuiltinType::Slice => Some(RecursiveConstruction::Slice),
            BuiltinType::Syntax if declaration.kind == staple_syntax::TypeDeclarationKind::Distinct => {
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
        self.package_interfaces = (0..program.modules().len())
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
                            if block.visibility != Visibility::Private {
                                self.insert_visible_value(
                                    source_module.id,
                                    &binding.name,
                                    symbol,
                                    block.visibility,
                                    binding.syntax.span.clone(),
                                );
                            }
                        }
                    }
                    Item::TypeDeclaration(declaration) => {
                        self.register_type_declaration(source_module.id, declaration, true);
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
                        if declaration.visibility != Visibility::Private {
                            self.package_interfaces[source_module.id.0]
                                .macros
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
                                functional_dependencies: Vec::new(),
                                methods,
                                default_methods: HashMap::new(),
                            },
                        );
                        if declaration.visibility != Visibility::Private {
                            self.package_interfaces[source_module.id.0]
                                .traits
                                .insert(declaration.name.clone(), id);
                            if declaration.visibility == Visibility::Public {
                                self.interfaces[source_module.id.0]
                                    .traits
                                    .insert(declaration.name.clone(), id);
                            }
                        }
                    }
                    Item::TraitImplementation(_) => {}
                    Item::Binding(binding) => {
                        let symbol = self.allocate_symbol(binding);
                        self.module_values[source_module.id.0].insert(binding.name.clone(), symbol);
                        if binding.visibility != Visibility::Private {
                            self.insert_visible_value(
                                source_module.id,
                                &binding.name,
                                symbol,
                                binding.visibility,
                                binding.syntax.span.clone(),
                            );
                        }
                    }
                    Item::PatternBinding(binding) => {
                        self.allocate_pattern_symbols(&binding.pattern);
                    }
                    Item::Assignment(_)
                    | Item::Return(_)
                    | Item::Break(_)
                    | Item::Continue(_)
                    | Item::Expression(_) => {}
                    Item::Submodule(submodule) => {
                        if submodule.visibility != Visibility::Private
                            && let Some(child) = program.child_module(submodule.syntax.id)
                        {
                            self.package_interfaces[source_module.id.0]
                                .namespaces
                                .insert(submodule.name.clone(), child);
                            if submodule.visibility == Visibility::Public {
                                self.interfaces[source_module.id.0]
                                    .namespaces
                                    .insert(submodule.name.clone(), child);
                            }
                        }
                    }
                    Item::Modified(_)
                    | Item::VisibilityMacroInvocation(_)
                    | Item::VisibilitySplice(_)
                    | Item::RepeatedItemSplice(_)
                    | Item::UseDeclaration(_) => {}
                }
            }
            let mut block_type_declarations = Vec::new();
            find_block_type_declarations(&source_module.syntax.items, &mut block_type_declarations);
            for declaration in block_type_declarations {
                self.register_type_declaration(source_module.id, declaration, false);
            }
            for item in &source_module.syntax.items {
                if let Item::Submodule(submodule) = item
                    && !submodule.companion
                    && self.declared_types[source_module.id.0].contains_key(&submodule.name)
                {
                    self.diagnostics.push(Diagnostic::new(
                        submodule.syntax.span.clone(),
                        format!(
                            "module `{}` conflicts with a type of the same name; use `companion` to add type items",
                            submodule.name
                        ),
                    ));
                }
            }
        }
        self.collect_reexports(program);
    }

    fn register_type_declaration(
        &mut self,
        module: ModuleId,
        declaration: &TypeDeclaration,
        top_level: bool,
    ) -> TypeId {
        let id = TypeId(self.type_declarations.len());
        self.type_ids_by_syntax.insert(declaration.syntax.id, id);
        // A block-scoped type's name is only ever looked up through
        // `block_types` (installed by `declare_block_type`), never through
        // this module-wide map — inserting it here too would make sibling
        // blocks reusing the same type name collide with each other, the
        // same way block-scoped `mod` avoids `children[parent]`.
        if top_level
            && self.declared_types[module.0]
                .insert(declaration.name.clone(), id)
                .is_some()
        {
            self.diagnostics.push(Diagnostic::new(
                declaration.syntax.span.clone(),
                format!("duplicate type definition of `{}`", declaration.name),
            ));
        }
        self.type_declarations.insert(id, declaration.clone());
        self.type_modules.insert(id, module);
        if (declaration.kind == staple_syntax::TypeDeclarationKind::Distinct
            && declaration.underlying.is_some())
            || declaration.kind == staple_syntax::TypeDeclarationKind::Singleton
        {
            let symbol = SymbolId(self.next_symbol_id);
            self.next_symbol_id += 1;
            self.symbol_owners.insert(symbol, None);
            self.type_constructor_symbols.insert(id, symbol);
            if top_level {
                self.module_values[module.0].insert(declaration.name.clone(), symbol);
            }
            if declaration.kind == staple_syntax::TypeDeclarationKind::Singleton {
                self.singleton_values.insert(symbol, id);
            } else {
                self.constructors.insert(symbol, id);
            }
            let constructor_visibility =
                if declaration.kind == staple_syntax::TypeDeclarationKind::Singleton {
                    declaration.visibility
                } else {
                    declaration.representation_visibility
                };
            if constructor_visibility != Visibility::Private {
                self.insert_visible_value(
                    module,
                    &declaration.name,
                    symbol,
                    constructor_visibility,
                    declaration.syntax.span.clone(),
                );
            }
        }
        let qualified = if self.multiple_modules {
            format!("m{}.{}", module.0, declaration.name)
        } else {
            declaration.name.clone()
        };
        self.type_names.insert(id, qualified);
        if declaration.visibility != Visibility::Private {
            self.package_interfaces[module.0]
                .types
                .insert(declaration.name.clone(), id);
            if declaration.visibility == Visibility::Public {
                self.interfaces[module.0]
                    .types
                    .insert(declaration.name.clone(), id);
            }
        }
        id
    }

    fn build_definition_context_values(&mut self, program: &Program) {
        self.definition_context_values = self.module_values.clone();
        self.definition_context_types = self.declared_types.clone();
        self.definition_context_namespaces = (0..program.modules().len())
            .map(|_| HashMap::new())
            .collect();
        // Unlike an ordinary inline module, a companion is an extension of
        // its declaring module's type namespace. Its body therefore sees the
        // parent's declarations without spelling `use super.*`.
        for module in program.modules() {
            if module.companion
                && let Some(parent) = module.parent
            {
                self.definition_context_values[module.id.0]
                    .extend(self.module_values[parent.0].clone());
                self.definition_context_types[module.id.0]
                    .extend(self.declared_types[parent.0].clone());
            }
        }
        for module in program.modules() {
            for (namespace, target) in program.root_qualified_modules(module.id) {
                self.definition_context_namespaces[module.id.0]
                    .insert(namespace.to_owned(), target);
            }
            for item in &module.syntax.items {
                if let Item::Submodule(submodule) = item
                    && let Some(child) = program.child_module(submodule.syntax.id)
                {
                    self.definition_context_namespaces[module.id.0]
                        .insert(submodule.name.clone(), child);
                }
            }
        }
        // A companion also sees the parent's submodule namespaces, the same
        // as it sees the parent's values and types above. The companion's
        // own namespaces take priority over the parent's on a name clash.
        for module in program.modules() {
            if module.companion
                && let Some(parent) = module.parent
            {
                let own = std::mem::take(&mut self.definition_context_namespaces[module.id.0]);
                let mut namespaces = self.definition_context_namespaces[parent.0].clone();
                namespaces.extend(own);
                self.definition_context_namespaces[module.id.0] = namespaces;
            }
        }
        if let Some(core) = program.standard_library_core() {
            for module in program.modules() {
                if module.id != core {
                    self.definition_context_values[module.id.0]
                        .extend(self.interfaces[core.0].values.clone());
                    self.definition_context_types[module.id.0]
                        .extend(self.interfaces[core.0].types.clone());
                    self.definition_context_namespaces[module.id.0]
                        .extend(self.interfaces[core.0].namespaces.clone());
                }
            }
        }
        if let Some(prelude) = program.standard_library_prelude() {
            for module in program.modules() {
                if module.id != prelude && program.module_uses_prelude(module.id) {
                    self.definition_context_values[module.id.0]
                        .extend(self.interfaces[prelude.0].values.clone());
                    self.definition_context_types[module.id.0]
                        .extend(self.interfaces[prelude.0].types.clone());
                    self.definition_context_namespaces[module.id.0]
                        .extend(self.interfaces[prelude.0].namespaces.clone());
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
                let interface = self.visible_interface(module.id, imported, use_.visibility);
                match self.use_kind(use_) {
                    UseKind::Dotted => {}
                    UseKind::Glob => {
                        self.definition_context_values[module.id.0]
                            .extend(interface.values.clone());
                        self.definition_context_types[module.id.0].extend(interface.types.clone());
                        self.definition_context_namespaces[module.id.0]
                            .extend(interface.namespaces.clone());
                    }
                    UseKind::Selected(names) => {
                        for import in names {
                            let alias = import.alias().to_owned();
                            if let Some(symbol) = interface.values.get(&import.name) {
                                self.definition_context_values[module.id.0]
                                    .insert(alias.clone(), *symbol);
                            }
                            if let Some(ty) = interface.types.get(&import.name) {
                                self.definition_context_types[module.id.0]
                                    .insert(alias.clone(), *ty);
                            }
                            if let Some(namespace) = interface.namespaces.get(&import.name) {
                                self.definition_context_namespaces[module.id.0]
                                    .insert(alias, *namespace);
                            }
                        }
                    }
                    UseKind::Renamed { item, alias } => {
                        if let Some(symbol) = interface.values.get(&item) {
                            self.definition_context_values[module.id.0]
                                .insert(alias.clone(), *symbol);
                        }
                        if let Some(ty) = interface.types.get(&item) {
                            self.definition_context_types[module.id.0].insert(alias.clone(), *ty);
                        }
                        if let Some(namespace) = interface.namespaces.get(&item) {
                            self.definition_context_namespaces[module.id.0]
                                .insert(alias.clone(), *namespace);
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
        self.binding_subtype_bounds
            .insert(binding.syntax.id, binding.subtype_bounds.clone());
        self.symbols.insert(binding.syntax.id, symbol);
        self.symbol_owners.insert(symbol, None);
        self.symbol_modules.insert(symbol, self.current_module);
        if binding.mutable {
            self.mutable_symbols.insert(symbol);
            self.mutable_annotations.insert(symbol);
        }
        if binding.signal {
            self.signal_symbols.insert(symbol);
        }
        if binding.kind == BindingKind::Const {
            self.const_symbols.insert(symbol);
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
                    self.mutable_annotations.insert(symbol);
                }
            }
            Pattern::At(at) => {
                let binding = &at.binding;
                let symbol = SymbolId(self.next_symbol_id);
                self.next_symbol_id += 1;
                self.declared_symbols.insert(binding.syntax.id, symbol);
                self.symbol_declarations.insert(symbol, binding.syntax.id);
                self.symbols.insert(binding.syntax.id, symbol);
                self.symbol_owners.insert(symbol, None);
                self.symbol_modules.insert(symbol, self.current_module);
                if binding.mutable {
                    self.mutable_symbols.insert(symbol);
                    self.mutable_annotations.insert(symbol);
                }
                self.allocate_pattern_symbols(&at.pattern);
            }
            Pattern::Product(product) => {
                for element in &product.elements {
                    self.allocate_pattern_symbols(element);
                }
            }
            Pattern::Nominal(pattern) => self.allocate_pattern_symbols(&pattern.argument),
        }
    }

    fn insert_visible_value(
        &mut self,
        module: ModuleId,
        name: &str,
        symbol: SymbolId,
        visibility: Visibility,
        span: Span,
    ) {
        self.package_interfaces[module.0]
            .values
            .insert(name.to_owned(), symbol);
        if visibility != Visibility::Public {
            return;
        }
        self.insert_public_value(module, name, symbol, span);
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
            if let Item::UseDeclaration(declaration) = item {
                self.install_import(declaration);
            }
        }
    }

    fn is_ancestor(&self, ancestor: ModuleId, mut module: ModuleId) -> bool {
        while let Some(&parent) = self.module_parents.get(&module) {
            if parent == ancestor {
                return true;
            }
            module = parent;
        }
        false
    }

    fn same_package(&self, left: ModuleId, right: ModuleId) -> bool {
        self.module_packages[left.0]
            .zip(self.module_packages[right.0])
            .is_some_and(|(left, right)| left == right)
    }

    fn representation_visible(&self, id: TypeId, from: ModuleId) -> bool {
        let declaration = &self.type_declarations[&id];
        let visibility = if declaration.kind == staple_syntax::TypeDeclarationKind::Singleton {
            declaration.visibility
        } else {
            declaration.representation_visibility
        };
        let Some(defining) = self.type_modules.get(&id).copied() else {
            return false;
        };
        visibility == Visibility::Public
            || (visibility == Visibility::Package && self.same_package(defining, from))
            || defining == from
            || self.is_ancestor(defining, from)
    }

    fn visible_interface(
        &self,
        importing: ModuleId,
        imported: ModuleId,
        import_visibility: Visibility,
    ) -> Interface {
        if import_visibility == Visibility::Public {
            self.interfaces[imported.0].clone()
        } else if self.is_ancestor(imported, importing) {
            self.local_interface(imported)
        } else if self.same_package(importing, imported) {
            self.package_interfaces[imported.0].clone()
        } else {
            self.interfaces[imported.0].clone()
        }
    }

    fn qualified_interface(&self, module: ModuleId) -> &Interface {
        if self.same_package(self.current_module, module) {
            &self.package_interfaces[module.0]
        } else {
            &self.interfaces[module.0]
        }
    }

    fn install_import(&mut self, declaration: &UseDeclaration) {
        let Some(imported) = self
            .imported_module_ids
            .get(&declaration.syntax.id)
            .copied()
        else {
            return;
        };
        let interface =
            self.visible_interface(self.current_module, imported, declaration.visibility);
        if self.use_kind(declaration) == UseKind::Glob {
            self.record_private_glob_items(imported, declaration, &interface);
        }
        // Only a namespace import's final path component actually names the
        // imported module (`io` in `use std.io`). For selected, renamed, and
        // dotted item imports the final component is an item name (`println`
        // in `use std.io.println`), so associating it with the containing
        // module would make go-to-definition on the item also jump to that
        // module's `pub mod` line.
        if self.use_kind(declaration) == UseKind::Namespace
            && let Some(name) = declaration.path.last()
        {
            self.import_definitions
                .entry((declaration.syntax.id, name.clone()))
                .or_default()
                .push(DefinitionId::Module(imported));
        }
        match self.use_kind(declaration) {
            UseKind::Dotted => {}
            UseKind::Namespace => {
                let name = declaration
                    .path
                    .last()
                    .expect("use path is nonempty")
                    .clone();
                if self
                    .namespaces
                    .last_mut()
                    .expect("resolver namespace scope")
                    .insert(name.clone(), imported)
                    .is_some()
                    || self.current_scope().contains_key(&name)
                {
                    self.duplicate_import(&name, declaration.syntax.span.clone());
                }
            }
            UseKind::Glob => {
                for (name, symbol) in interface.values.clone() {
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
                for (name, namespace) in interface.namespaces.clone() {
                    if interface.types.contains_key(&name) {
                        if self
                            .namespaces
                            .last_mut()
                            .expect("resolver namespace scope")
                            .insert(name.clone(), namespace)
                            .is_some()
                        {
                            self.duplicate_import(&name, declaration.syntax.span.clone());
                        }
                    } else {
                        self.insert_imported_namespace(
                            name,
                            namespace,
                            declaration.syntax.span.clone(),
                        );
                    }
                }
            }
            UseKind::Selected(names) => {
                for import in names {
                    let alias = import.alias().to_owned();
                    self.record_import_definitions(
                        declaration.syntax.id,
                        &import.name,
                        &alias,
                        &interface,
                    );
                    self.install_selected(
                        &interface,
                        &import.name,
                        &alias,
                        declaration.syntax.span.clone(),
                    );
                    if let Some(namespace) = self
                        .additional_imported_namespaces
                        .get(&declaration.syntax.id)
                        .copied()
                        && interface.namespaces.get(&import.name) != Some(&namespace)
                    {
                        self.insert_imported_namespace(
                            alias.clone(),
                            namespace,
                            declaration.syntax.span.clone(),
                        );
                        self.import_definitions
                            .entry((declaration.syntax.id, alias))
                            .or_default()
                            .push(DefinitionId::Module(namespace));
                    }
                }
            }
            UseKind::Renamed { item, alias } => {
                self.record_import_definitions(declaration.syntax.id, &item, &alias, &interface);
                self.install_selected(&interface, &item, &alias, declaration.syntax.span.clone());
            }
        }
    }

    fn use_kind(&self, declaration: &UseDeclaration) -> UseKind {
        self.resolved_use_kinds
            .get(&declaration.syntax.id)
            .cloned()
            .unwrap_or_else(|| declaration.kind.clone())
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
        if let Some(module) = interface.namespaces.get(item).copied() {
            definitions.push(DefinitionId::Module(module));
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
            let previous = self
                .namespaces
                .last_mut()
                .expect("resolver namespace scope")
                .insert(submodule.name.clone(), child);
            if previous.is_some_and(|previous| previous != child) {
                self.duplicate_import(&submodule.name, submodule.syntax.span.clone());
            }
        }
    }

    fn install_root_namespaces(&mut self, program: &Program, module: ModuleId) {
        for (namespace, target) in program.root_qualified_modules(module) {
            self.namespaces
                .last_mut()
                .expect("resolver namespace scope")
                .insert(namespace.to_owned(), target);
        }
    }

    fn local_interface(&self, module: ModuleId) -> Interface {
        Interface {
            values: self.module_values[module.0].clone(),
            types: self.declared_types[module.0].clone(),
            macros: self.declared_macros[module.0].clone(),
            traits: self.declared_traits[module.0].clone(),
            namespaces: self.interfaces[module.0].namespaces.clone(),
        }
    }

    fn record_private_glob_items(
        &mut self,
        module: ModuleId,
        declaration: &staple_syntax::UseDeclaration,
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
        self.prelude_macros = self.interfaces[core.0].macros.clone();
        self.prelude_traits = self.interfaces[core.0].traits.clone();
        self.prelude_namespaces = self.interfaces[core.0].namespaces.clone();
        if program.module_uses_prelude(module)
            && let Some(prelude) = program.standard_library_prelude()
            && module != prelude
        {
            self.prelude_values
                .extend(self.interfaces[prelude.0].values.clone());
            self.prelude_types
                .extend(self.interfaces[prelude.0].types.clone());
            self.prelude_macros
                .extend(self.interfaces[prelude.0].macros.clone());
            self.prelude_traits
                .extend(self.interfaces[prelude.0].traits.clone());
            self.prelude_namespaces
                .extend(self.interfaces[prelude.0].namespaces.clone());
        }
        for trait_id in self.prelude_traits.values().copied().collect::<Vec<_>>() {
            self.add_visible_trait_methods(trait_id);
        }
    }

    fn install_selected(&mut self, interface: &Interface, item: &str, local: &str, span: Span) {
        let mut found = false;
        if let Some(symbol) = interface.values.get(item).copied() {
            found = true;
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
        if let Some(namespace) = interface.namespaces.get(item).copied() {
            found = true;
            if interface.types.contains_key(item) {
                if self
                    .namespaces
                    .last_mut()
                    .expect("resolver namespace scope")
                    .insert(local.to_owned(), namespace)
                    .is_some()
                {
                    self.duplicate_import(local, span.clone());
                }
            } else {
                self.insert_imported_namespace(local.to_owned(), namespace, span.clone());
            }
        }
        if !found {
            self.diagnostics.push(Diagnostic::new(
                span,
                format!("module has no public item named `{item}`"),
            ));
        }
    }

    fn insert_imported_value(&mut self, name: String, symbol: SymbolId, span: Span) {
        if self.current_scope().contains_key(&name)
            || self
                .namespaces
                .iter()
                .any(|frame| frame.contains_key(&name))
        {
            self.duplicate_import(&name, span);
        } else {
            self.current_scope_mut().insert(name, symbol);
        }
    }

    fn insert_imported_type(&mut self, name: String, ty: TypeId, span: Span) {
        if self.declared_types[self.current_module.0].contains_key(&name)
            || self
                .block_types
                .last()
                .is_some_and(|frame| frame.contains_key(&name))
            || self
                .imported_types
                .last_mut()
                .expect("resolver type import scope")
                .insert(name.clone(), ty)
                .is_some()
        {
            self.duplicate_import(&name, span);
        }
    }

    fn insert_imported_macro(&mut self, name: String, id: Vec<MacroId>, span: Span) {
        if self.current_scope().contains_key(&name)
            || self
                .namespaces
                .iter()
                .any(|frame| frame.contains_key(&name))
            || self
                .imported_macros
                .last_mut()
                .expect("resolver macro import scope")
                .insert(name.clone(), id)
                .is_some()
        {
            self.duplicate_import(&name, span);
        }
    }

    fn insert_imported_trait(&mut self, name: String, id: TraitId, span: Span) {
        if self.declared_traits[self.current_module.0].contains_key(&name)
            || self
                .imported_traits
                .last_mut()
                .expect("resolver trait import scope")
                .insert(name.clone(), id)
                .is_some()
        {
            self.duplicate_import(&name, span);
        } else {
            self.add_visible_trait_methods(id);
        }
    }

    fn insert_imported_namespace(&mut self, name: String, module: ModuleId, span: Span) {
        if self
            .namespaces
            .last_mut()
            .expect("resolver namespace scope")
            .insert(name.clone(), module)
            .is_some()
            || self.current_scope().contains_key(&name)
        {
            self.duplicate_import(&name, span);
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
                        self.declare_allocated(binding, None);
                    }
                }
                Item::Binding(binding)
                    if matches!(binding.kind, BindingKind::Def | BindingKind::Const) =>
                {
                    self.declare_allocated(binding, None);
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
                | Item::TraitImplementation(_)
                | Item::Binding(_)
                | Item::PatternBinding(_)
                | Item::Assignment(_)
                | Item::Return(_)
                | Item::Break(_)
                | Item::Continue(_)
                | Item::Expression(_) => {}
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
        for (name, id) in self.declared_types[module.0]
            .iter()
            .chain(self.prelude_types.iter())
        {
            insert(name, DefinitionId::Type(*id));
        }
        for (name, id) in self.imported_types.iter().flatten() {
            insert(name, DefinitionId::Type(*id));
        }
        for (name, id) in self.declared_traits[module.0]
            .iter()
            .chain(self.prelude_traits.iter())
        {
            insert(name, DefinitionId::Trait(*id));
        }
        for (name, id) in self.imported_traits.iter().flatten() {
            insert(name, DefinitionId::Trait(*id));
        }
        for (name, ids) in self.declared_macros[module.0]
            .iter()
            .chain(self.prelude_macros.iter())
        {
            for id in ids {
                insert(name, DefinitionId::Macro(*id));
            }
        }
        for (name, ids) in self.imported_macros.iter().flatten() {
            for id in ids {
                insert(name, DefinitionId::Macro(*id));
            }
        }
        for (name, id) in &self.prelude_namespaces {
            insert(name, DefinitionId::Module(*id));
        }
        for (name, id) in self.namespaces.iter().flatten() {
            insert(name, DefinitionId::Module(*id));
        }
        for (name, ids) in &self.visible_trait_methods {
            for id in ids {
                insert(name, DefinitionId::TraitMethod(*id));
            }
        }
        self.visible_module_definitions[module.0] = visible;
    }

    fn resolve_type_declaration_body(&mut self, declaration: &TypeDeclaration) {
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
        for bound in &declaration.subtype_bounds {
            if let Some(id) = self.lookup_type_parameter(&bound.parameter.name) {
                self.type_parameters.insert(bound.syntax.id, id);
            } else {
                self.diagnostics.push(Diagnostic::new(
                    bound.parameter.syntax.span.clone(),
                    format!(
                        "unknown compile-time parameter `{}` in subtype bound",
                        bound.parameter.name
                    ),
                ));
            }
            self.resolve_type(&bound.supertype);
        }
        let mut defaulted_parameters = HashSet::new();
        for bound in &declaration.default_bounds {
            if let Some(id) = self.lookup_type_parameter(&bound.parameter.name) {
                self.type_parameters.insert(bound.syntax.id, id);
                if !defaulted_parameters.insert(id) {
                    self.diagnostics.push(Diagnostic::new(
                        bound.parameter.syntax.span.clone(),
                        format!(
                            "duplicate default type bound for compile-time parameter `{}`",
                            bound.parameter.name
                        ),
                    ));
                }
            } else {
                self.diagnostics.push(Diagnostic::new(
                    bound.parameter.syntax.span.clone(),
                    format!(
                        "unknown compile-time parameter `{}` in default type bound",
                        bound.parameter.name
                    ),
                ));
            }
            self.resolve_type(&bound.default);
        }
        if let Some(underlying) = &declaration.underlying {
            self.resolve_type(underlying);
            if declaration.representation_visibility != Visibility::Private {
                self.validate_representation(underlying, declaration.representation_visibility);
            }
        }
        self.pop_type_parameter_scope();
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
                self.resolve_type_declaration_body(declaration);
            }
            Item::MacroDeclaration(declaration) => {
                self.resolve_macro_declaration(declaration);
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
                let mut functional_dependencies = Vec::new();
                for dependency in &declaration.functional_dependencies {
                    let mut determinants = Vec::new();
                    let mut seen = HashSet::new();
                    for determinant in &dependency.determinants {
                        let Some(parameter) = self.lookup_type_parameter(&determinant.name) else {
                            self.diagnostics.push(Diagnostic::new(
                                determinant.syntax.span.clone(),
                                format!(
                                    "unknown trait type parameter `{}` in functional dependency",
                                    determinant.name
                                ),
                            ));
                            continue;
                        };
                        self.type_parameters
                            .insert(determinant.syntax.id, parameter);
                        if !seen.insert(parameter) {
                            self.diagnostics.push(Diagnostic::new(
                                determinant.syntax.span.clone(),
                                format!(
                                    "duplicate functional dependency determinant `{}`",
                                    determinant.name
                                ),
                            ));
                            continue;
                        }
                        determinants.push(parameter);
                    }
                    let Some(dependent) = self.lookup_type_parameter(&dependency.dependent.name)
                    else {
                        self.diagnostics.push(Diagnostic::new(
                            dependency.dependent.syntax.span.clone(),
                            format!(
                                "unknown trait type parameter `{}` in functional dependency",
                                dependency.dependent.name
                            ),
                        ));
                        continue;
                    };
                    self.type_parameters
                        .insert(dependency.dependent.syntax.id, dependent);
                    if seen.contains(&dependent) {
                        self.diagnostics.push(Diagnostic::new(
                            dependency.dependent.syntax.span.clone(),
                            "functional dependency cannot determine one of its determinants",
                        ));
                        continue;
                    }
                    if determinants.is_empty() {
                        continue;
                    }
                    functional_dependencies.push(ResolvedFunctionalDependency {
                        determinants,
                        dependent,
                    });
                }
                self.traits
                    .get_mut(&trait_id)
                    .expect("resolved trait")
                    .functional_dependencies = functional_dependencies;
                for prerequisite in &declaration.prerequisites {
                    if let Some(trait_id) = self.resolve_trait_name(&prerequisite.trait_name) {
                        self.trait_references
                            .insert(prerequisite.syntax.id, trait_id);
                    }
                    for argument in &prerequisite.arguments {
                        self.resolve_type(argument);
                    }
                }
                let mut defaulted_parameters = HashSet::new();
                for bound in &declaration.default_bounds {
                    if let Some(id) = self.lookup_type_parameter(&bound.parameter.name) {
                        self.type_parameters.insert(bound.syntax.id, id);
                        if !defaulted_parameters.insert(id) {
                            self.diagnostics.push(Diagnostic::new(
                                bound.parameter.syntax.span.clone(),
                                format!(
                                    "duplicate default type bound for compile-time parameter `{}`",
                                    bound.parameter.name
                                ),
                            ));
                        }
                    } else {
                        self.diagnostics.push(Diagnostic::new(
                            bound.parameter.syntax.span.clone(),
                            format!(
                                "unknown compile-time parameter `{}` in default type bound",
                                bound.parameter.name
                            ),
                        ));
                    }
                    self.resolve_type(&bound.default);
                }
                let mut default_methods = HashMap::new();
                for member in &declaration.members {
                    self.resolve_type(&member.annotation);
                    if declaration.visibility == Visibility::Public {
                        self.validate_representation(&member.annotation, Visibility::Public);
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
                self.syntax_modules
                    .insert(implementation.syntax.id, self.current_module);
                self.push_type_parameter_scope();
                let mut parameters = Vec::new();
                for parameter in &implementation.type_parameters {
                    self.declare_type_parameter_pattern(parameter);
                    self.collect_declared_type_parameter_pattern(parameter, &mut parameters);
                }
                let trait_id = self.resolve_trait_name(&implementation.trait_name);
                for argument in &implementation.arguments {
                    self.resolve_type(argument);
                }
                for bound in &implementation.trait_bounds {
                    if let Some(bound_trait_id) = self.resolve_trait_name(&bound.trait_name) {
                        self.trait_references
                            .insert(bound.syntax.id, bound_trait_id);
                    }
                    for argument in &bound.arguments {
                        self.resolve_type(argument);
                    }
                }
                for bound in &implementation.subtype_bounds {
                    if let Some(id) = self.lookup_type_parameter(&bound.parameter.name) {
                        self.type_parameters.insert(bound.syntax.id, id);
                    } else {
                        self.diagnostics.push(Diagnostic::new(
                            bound.parameter.syntax.span.clone(),
                            format!(
                                "unknown compile-time parameter `{}` in subtype bound",
                                bound.parameter.name
                            ),
                        ));
                    }
                    self.resolve_type(&bound.supertype);
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
                    self.binding_type_parameters
                        .insert(member.syntax.id, implementation.type_parameters.clone());
                    self.binding_trait_bounds
                        .insert(member.syntax.id, implementation.trait_bounds.clone());
                    self.binding_subtype_bounds
                        .insert(member.syntax.id, implementation.subtype_bounds.clone());
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
                            parameters,
                            arguments: implementation.arguments.clone(),
                            trait_bounds: implementation.trait_bounds.clone(),
                            subtype_bounds: implementation.subtype_bounds.clone(),
                            negative: implementation.negative,
                            methods,
                        });
                }
                self.pop_type_parameter_scope();
            }
            item @ (Item::Binding(_)
            | Item::PatternBinding(_)
            | Item::Assignment(_)
            | Item::Return(_)
            | Item::Break(_)
            | Item::Continue(_)
            | Item::Expression(_)) => self.resolve_block_item(item),
        }
    }

    fn resolve_block_item(&mut self, item: &Item) {
        match item {
            Item::Binding(binding) => self.resolve_binding(binding),
            Item::PatternBinding(binding) => {
                if binding.kind == PatternBindingKind::Propagating {
                    if self.function_stack.is_empty() {
                        self.diagnostics.push(Diagnostic::new(
                            binding.syntax.span.clone(),
                            "propagating bindings are only allowed inside a function",
                        ));
                    }
                    let mut root = &binding.pattern;
                    while let Pattern::At(at) = root {
                        root = &at.pattern;
                    }
                    if !matches!(root, Pattern::Nominal(_)) {
                        self.diagnostics.push(Diagnostic::new(
                            binding.pattern.syntax().span.clone(),
                            "a propagating binding requires a nominal pattern",
                        ));
                    }
                }
                self.resolve_pattern_types(&binding.pattern);
                self.resolve_expression(&binding.value, None, None);
                self.declare_pattern(&binding.pattern, Some(false), &mut HashSet::new());
            }
            Item::Assignment(assignment) => {
                self.syntax_modules
                    .insert(assignment.syntax.id, self.current_module);
                self.resolve_expression(&assignment.target, None, None);
                self.resolve_expression(&assignment.value, None, None);
            }
            Item::Return(item) => {
                if self.function_stack.is_empty() {
                    self.diagnostics.push(Diagnostic::new(
                        item.syntax.span.clone(),
                        "`return` is only allowed inside a function",
                    ));
                }
                self.resolve_expression(&item.value, None, None);
            }
            Item::Break(item) => {
                if self.loop_depth == 0 {
                    self.diagnostics.push(Diagnostic::new(
                        item.syntax.span.clone(),
                        "`break` is only allowed inside a loop",
                    ));
                }
                if let Some(value) = &item.value {
                    self.resolve_expression(value, None, None);
                }
            }
            Item::Continue(item) => {
                if self.loop_depth == 0 {
                    self.diagnostics.push(Diagnostic::new(
                        item.syntax.span.clone(),
                        "`continue` is only allowed inside a loop",
                    ));
                }
            }
            Item::Expression(expression) => self.resolve_expression(expression, None, None),
            Item::Submodule(_) => {}
            Item::TypeDeclaration(declaration) => {
                self.resolve_type_declaration_body(declaration);
            }
            Item::MacroDeclaration(declaration) => {
                self.resolve_macro_declaration(declaration);
            }
            Item::UseDeclaration(_) => {}
            _ => {}
        }
    }

    fn resolve_binding(&mut self, binding: &Binding) {
        self.binding_type_parameters
            .entry(binding.syntax.id)
            .or_insert_with(|| binding.type_parameters.clone());
        self.binding_trait_bounds
            .entry(binding.syntax.id)
            .or_insert_with(|| binding.trait_bounds.clone());
        self.binding_subtype_bounds
            .entry(binding.syntax.id)
            .or_insert_with(|| binding.subtype_bounds.clone());
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
        for bound in &binding.subtype_bounds {
            if let Some(id) = self.lookup_type_parameter(&bound.parameter.name) {
                self.type_parameters.insert(bound.syntax.id, id);
            } else {
                self.diagnostics.push(Diagnostic::new(
                    bound.parameter.syntax.span.clone(),
                    format!(
                        "unknown compile-time parameter `{}` in subtype bound",
                        bound.parameter.name
                    ),
                ));
            }
            self.resolve_type(&bound.supertype);
        }
        if let Some(value) = &binding.value {
            self.resolve_expression(
                value,
                binding.annotation.as_ref(),
                Some((&binding.name, binding.syntax.id)),
            );
        }
        if binding.kind == BindingKind::Let {
            self.declare_allocated(binding, Some(binding.visibility == Visibility::Public));
        }
        self.pop_type_parameter_scope();
    }

    fn resolve_macro_declaration(&mut self, declaration: &MacroDeclaration) {
        self.push_type_parameter_scope();
        for parameter in &declaration.type_parameters {
            self.declare_type_parameter_pattern(parameter);
        }
        if let Some(annotation) = &declaration.annotation {
            self.resolve_type_lenient(annotation);
        }
        for bound in &declaration.trait_bounds {
            if let Some(trait_id) = self.resolve_trait_name(&bound.trait_name) {
                self.trait_references.insert(bound.syntax.id, trait_id);
            }
            for argument in &bound.arguments {
                self.resolve_type_lenient(argument);
            }
        }
        let mut current = declaration.value.as_ref();
        while let Some(Expression::Function(function)) = current {
            self.resolve_pattern_types_lenient(&function.pattern);
            current = Some(function.body.as_ref());
        }
        if let Some(value) = &declaration.value {
            self.resolve_compile_time_expression_annotations(value);
        }
        self.pop_type_parameter_scope();
    }

    fn resolve_compile_time_helper_annotations(&mut self, binding: &Binding) {
        self.push_type_parameter_scope();
        for parameter in &binding.type_parameters {
            self.declare_type_parameter_pattern(parameter);
        }
        if let Some(annotation) = &binding.annotation {
            self.resolve_type_lenient(annotation);
        }
        for bound in &binding.trait_bounds {
            if let Some(trait_id) = self.resolve_trait_name(&bound.trait_name) {
                self.trait_references.insert(bound.syntax.id, trait_id);
            }
            for argument in &bound.arguments {
                self.resolve_type_lenient(argument);
            }
        }
        if let Some(value) = &binding.value {
            self.resolve_compile_time_expression_annotations(value);
        }
        self.pop_type_parameter_scope();
    }

    fn resolve_compile_time_expression_annotations(&mut self, expression: &Expression) {
        match expression {
            Expression::Function(value) => {
                if pattern_contains_mutable_marker(&value.pattern) {
                    self.diagnostics.push(Diagnostic::new(
                        value.pattern.syntax().span.clone(),
                        "parameter `mut` effects are only supported by runtime functions",
                    ));
                }
                self.resolve_pattern_types_lenient(&value.pattern);
                self.resolve_compile_time_expression_annotations(&value.body);
            }
            Expression::Satisfies(value) => {
                self.resolve_compile_time_expression_annotations(&value.value);
                self.resolve_type_lenient(&value.ty);
            }
            Expression::Match(value) => {
                self.resolve_compile_time_expression_annotations(&value.subject);
                for arm in &value.arms {
                    self.resolve_pattern_types_lenient(&arm.pattern);
                    self.resolve_compile_time_expression_annotations(&arm.body);
                }
            }
            Expression::Loop(value) => {
                for item in &value.body.items {
                    self.resolve_compile_time_item_annotations(item);
                }
            }
            Expression::Resource(value) => self.resolve_type_lenient(&value.resource),
            Expression::With(value) => {
                self.resolve_type_lenient(&value.resource);
                self.resolve_compile_time_expression_annotations(&value.value);
                for item in &value.body.items {
                    self.resolve_compile_time_item_annotations(item);
                }
            }
            Expression::Block(value) => {
                for item in &value.items {
                    self.resolve_compile_time_item_annotations(item);
                }
            }
            Expression::Product(value) => {
                for element in &value.elements {
                    self.resolve_compile_time_expression_annotations(&element.value);
                }
            }
            Expression::RepeatedProduct(value) => {
                self.resolve_compile_time_expression_annotations(&value.value);
                self.resolve_compile_time_expression_annotations(&value.count);
            }
            Expression::Call(value) => {
                self.resolve_compile_time_expression_annotations(&value.callee);
                self.resolve_compile_time_expression_annotations(&value.argument);
            }
            Expression::Access(value) => {
                self.resolve_compile_time_expression_annotations(&value.value)
            }
            Expression::Index(value) => {
                self.resolve_compile_time_expression_annotations(&value.value);
                self.resolve_compile_time_expression_annotations(&value.index);
            }
            Expression::Quote(value) => {
                if let Some(id) = self
                    .lookup_macro(value.kind.name())
                    .and_then(|ids| ids.first().copied())
                {
                    self.quote_macros.insert(value.syntax.id, id);
                }
                match &value.template {
                    staple_syntax::QuoteTemplate::Expression(value) => {
                        self.resolve_compile_time_expression_annotations(value);
                        self.resolve_quoted_expression(value, &mut vec![HashMap::new()]);
                    }
                    staple_syntax::QuoteTemplate::Item(item) => {
                        self.resolve_compile_time_item_annotations(item);
                        self.resolve_quoted_item(item, &mut vec![HashMap::new()]);
                    }
                    staple_syntax::QuoteTemplate::Items(items) => {
                        let mut scopes = vec![HashMap::new()];
                        for item in items {
                            self.resolve_compile_time_item_annotations(item);
                            self.resolve_quoted_item(item, &mut scopes);
                        }
                    }
                    staple_syntax::QuoteTemplate::Raw => {}
                }
            }
            _ => {}
        }
    }

    fn resolve_compile_time_item_annotations(&mut self, item: &Item) {
        match item {
            Item::Binding(value) => {
                if let Some(annotation) = &value.annotation {
                    self.resolve_type_lenient(annotation);
                }
                if let Some(expression) = &value.value {
                    self.resolve_compile_time_expression_annotations(expression);
                }
            }
            Item::PatternBinding(value) => {
                self.resolve_pattern_types_lenient(&value.pattern);
                self.resolve_compile_time_expression_annotations(&value.value);
            }
            Item::Assignment(value) => {
                self.resolve_compile_time_expression_annotations(&value.target);
                self.resolve_compile_time_expression_annotations(&value.value);
            }
            Item::Return(value) => self.resolve_compile_time_expression_annotations(&value.value),
            Item::Break(value) => {
                if let Some(value) = &value.value {
                    self.resolve_compile_time_expression_annotations(value);
                }
            }
            Item::Expression(value) => self.resolve_compile_time_expression_annotations(value),
            _ => {}
        }
    }

    fn declare_quoted_pattern(&mut self, pattern: &Pattern, scope: &mut HashMap<String, SymbolId>) {
        match pattern {
            Pattern::Binding(binding) => {
                let symbol = SymbolId(self.next_symbol_id);
                self.next_symbol_id += 1;
                self.symbols.insert(binding.syntax.id, symbol);
                self.symbol_declarations.insert(symbol, binding.syntax.id);
                self.symbol_modules.insert(symbol, self.current_module);
                scope.insert(binding.name.clone(), symbol);
            }
            Pattern::At(at) => {
                self.declare_quoted_pattern(&Pattern::Binding(at.binding.as_ref().clone()), scope);
                self.declare_quoted_pattern(&at.pattern, scope);
            }
            Pattern::Product(product) => {
                for element in &product.elements {
                    self.declare_quoted_pattern(element, scope);
                }
            }
            Pattern::Nominal(nominal) => self.declare_quoted_pattern(&nominal.argument, scope),
            _ => {}
        }
    }

    fn resolve_quoted_expression(
        &mut self,
        expression: &Expression,
        scopes: &mut Vec<HashMap<String, SymbolId>>,
    ) {
        match expression {
            Expression::Name(name) => {
                if let Some(symbol) = scopes
                    .iter()
                    .rev()
                    .find_map(|scope| scope.get(&name.name))
                    .copied()
                    .or_else(|| self.lookup(&name.name))
                {
                    self.symbols.insert(name.syntax.id, symbol);
                }
            }
            Expression::Function(value) => {
                scopes.push(HashMap::new());
                self.declare_quoted_pattern(&value.pattern, scopes.last_mut().unwrap());
                self.resolve_quoted_expression(&value.body, scopes);
                scopes.pop();
            }
            Expression::Match(value) => {
                self.resolve_quoted_expression(&value.subject, scopes);
                for arm in &value.arms {
                    scopes.push(HashMap::new());
                    self.declare_quoted_pattern(&arm.pattern, scopes.last_mut().unwrap());
                    self.resolve_quoted_expression(&arm.body, scopes);
                    scopes.pop();
                }
            }
            Expression::Block(value) => {
                scopes.push(HashMap::new());
                for item in &value.items {
                    self.resolve_quoted_item(item, scopes);
                }
                scopes.pop();
            }
            Expression::Loop(value) => {
                scopes.push(HashMap::new());
                for item in &value.body.items {
                    self.resolve_quoted_item(item, scopes);
                }
                scopes.pop();
            }
            Expression::With(value) => {
                self.resolve_quoted_expression(&value.value, scopes);
                scopes.push(HashMap::new());
                for item in &value.body.items {
                    self.resolve_quoted_item(item, scopes);
                }
                scopes.pop();
            }
            Expression::Satisfies(value) => self.resolve_quoted_expression(&value.value, scopes),
            Expression::Product(value) => {
                for element in &value.elements {
                    self.resolve_quoted_expression(&element.value, scopes);
                }
            }
            Expression::RepeatedProduct(value) => {
                self.resolve_quoted_expression(&value.value, scopes);
                self.resolve_quoted_expression(&value.count, scopes);
            }
            Expression::Call(value) => {
                self.resolve_quoted_expression(&value.callee, scopes);
                self.resolve_quoted_expression(&value.argument, scopes);
            }
            Expression::Access(value) => {
                if let Some(trait_id) = self.trait_id_from_expression(&value.value)
                    && let Accessor::Name(name) = &value.accessor
                    && let Some(method) = self
                        .trait_member_ids
                        .get(&(trait_id, name.clone()))
                        .copied()
                {
                    self.trait_method_references
                        .insert(value.syntax.id, vec![method]);
                    if let Expression::Name(trait_name) = value.value.as_ref() {
                        self.trait_references.insert(trait_name.syntax.id, trait_id);
                    }
                } else if let Some((namespace, item, definition_module, _)) =
                    qualified_access_path(value)
                    && let Some(module) = definition_module
                        .map(ModuleId)
                        .or_else(|| self.lookup_namespace(&namespace))
                    && let Some(symbol) =
                        self.qualified_interface(module).values.get(&item).copied()
                {
                    self.symbols.insert(value.syntax.id, symbol);
                } else {
                    self.resolve_quoted_expression(&value.value, scopes);
                }
            }
            Expression::Index(value) => {
                self.resolve_quoted_expression(&value.value, scopes);
                self.resolve_quoted_expression(&value.index, scopes);
            }
            Expression::Logical(value) => {
                self.resolve_quoted_expression(&value.left, scopes);
                self.resolve_quoted_expression(&value.right, scopes);
            }
            Expression::Quote(_)
            | Expression::Splice(_)
            | Expression::Resource(_)
            | Expression::SyntaxArgument(_)
            | Expression::VisibilityArgument(_)
            | Expression::String(_)
            | Expression::StringTemplate(_)
            | Expression::CString(_)
            | Expression::Integer(_)
            | Expression::Float(_) => {}
            Expression::Binary(_) => unreachable!("binary expression was not desugared"),
        }
    }

    fn resolve_quoted_item(&mut self, item: &Item, scopes: &mut Vec<HashMap<String, SymbolId>>) {
        match item {
            Item::Binding(binding) => {
                if let Some(value) = &binding.value {
                    self.resolve_quoted_expression(value, scopes);
                }
                let symbol = SymbolId(self.next_symbol_id);
                self.next_symbol_id += 1;
                self.symbols.insert(binding.syntax.id, symbol);
                self.symbol_declarations.insert(symbol, binding.syntax.id);
                self.symbol_modules.insert(symbol, self.current_module);
                scopes
                    .last_mut()
                    .unwrap()
                    .insert(binding.name.clone(), symbol);
            }
            Item::PatternBinding(value) => {
                self.resolve_quoted_expression(&value.value, scopes);
                self.declare_quoted_pattern(&value.pattern, scopes.last_mut().unwrap());
            }
            Item::Assignment(value) => {
                self.resolve_quoted_expression(&value.target, scopes);
                self.resolve_quoted_expression(&value.value, scopes);
            }
            Item::Expression(value) => self.resolve_quoted_expression(value, scopes),
            Item::Return(value) => self.resolve_quoted_expression(&value.value, scopes),
            Item::Break(value) => {
                if let Some(value) = &value.value {
                    self.resolve_quoted_expression(value, scopes);
                }
            }
            _ => {}
        }
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

    fn collect_declared_type_parameter_pattern(
        &self,
        pattern: &TypeParameterPattern,
        parameters: &mut Vec<TypeParameterId>,
    ) {
        match pattern {
            TypeParameterPattern::Binding(binding) => {
                parameters.push(self.type_parameters[&binding.syntax.id]);
            }
            TypeParameterPattern::Effect(binding) => {
                parameters.push(self.type_parameters[&binding.syntax.id])
            }
            TypeParameterPattern::Product(product) => {
                for element in &product.elements {
                    self.collect_declared_type_parameter_pattern(element, parameters);
                }
            }
            TypeParameterPattern::Splice(_) => {
                unreachable!("type-parameter splices must be expanded before resolution")
            }
        }
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
            TypeParameterPattern::Effect(binding) => {
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
                self.type_parameter_sized.insert(id, false);
                self.effect_parameters.insert(id);
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
            TypeParameterPattern::Effect(binding) => {
                let id = TypeParameterId(self.next_type_parameter_id);
                self.next_type_parameter_id += 1;
                self.type_parameters.insert(binding.syntax.id, id);
                self.type_parameter_sized.insert(id, false);
                self.effect_parameters.insert(id);
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
            TypeParameterPattern::Effect(binding) => {
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
        self.syntax_modules
            .insert(expression.syntax().id, self.current_module);
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
                self.declare_pattern(&function.pattern, None, &mut HashSet::new());
                // A `mut` marker in a function parameter declares a mutation
                // effect on that arrow; it is not an ordinary mutable local
                // binding in the resolver's sense.
                for symbol in mutable_pattern_symbols(self, &function.pattern) {
                    self.mutable_symbols.remove(&symbol);
                }
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
                    || Some(self.current_module) == self.standard_library_syntax
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
                    parameter_style: function.parameter_style,
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
                    subtype_bounds: suggested_function
                        .and_then(|(_, syntax)| self.binding_subtype_bounds.get(&syntax).cloned())
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
                    self.declare_pattern(&arm.pattern, None, &mut HashSet::new());
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
            Expression::RepeatedProduct(repeated) => {
                self.resolve_expression(&repeated.value, None, None);
                self.resolve_expression(&repeated.count, None, None);
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
                        if let Expression::Name(trait_name) = access.value.as_ref() {
                            self.trait_references.insert(trait_name.syntax.id, trait_id);
                        }
                    } else {
                        self.diagnostics.push(Diagnostic::new(
                            access.syntax.span.clone(),
                            format!("trait has no member named `{member_name}`"),
                        ));
                    }
                } else if let Some((namespace, item, definition_module, segments)) =
                    qualified_access_path(access)
                    && let Some(module) = definition_module
                        .and_then(|context| {
                            self.definition_context_namespaces
                                .get(context)
                                .and_then(|namespaces| namespaces.get(&namespace))
                                .copied()
                        })
                        .or_else(|| self.lookup_namespace(&namespace))
                {
                    // `module` is the namespace the full chain resolved to
                    // (e.g. the `io` module for `std.io`). Walk each shorter
                    // prefix's node (`std`) back up the module tree via its
                    // parent. Inline submodules are linked through
                    // `module_parents`; separately-loaded package files are
                    // not, so when that link is missing recover the segment's
                    // module by resolving its own dotted prefix as a
                    // root-qualified namespace (the loader registers every
                    // resolvable prefix, e.g. `package.outer` as well as
                    // `package.outer.inner`).
                    let prefix_parts = namespace.split('.').collect::<Vec<_>>();
                    let mut segment_module = Some(module);
                    for (index, syntax_id) in segments.iter().enumerate().rev() {
                        let current = segment_module.or_else(|| {
                            let prefix = prefix_parts.get(..=index)?.join(".");
                            definition_module
                                .and_then(|context| {
                                    self.definition_context_namespaces
                                        .get(context)
                                        .and_then(|namespaces| namespaces.get(&prefix))
                                        .copied()
                                })
                                .or_else(|| self.lookup_namespace(&prefix))
                        });
                        let Some(current) = current else {
                            break;
                        };
                        self.namespace_references.insert(*syntax_id, current);
                        segment_module = self.module_parents.get(&current).copied();
                    }
                    if let Some(symbol) =
                        self.qualified_interface(module).values.get(&item).copied()
                    {
                        self.symbols.insert(access.syntax.id, symbol);
                    } else if self.qualified_interface(module).macros.contains_key(&item) {
                        self.diagnostics.push(Diagnostic::new(
                            access.syntax.span.clone(),
                            format!("macro `{item}` must be invoked"),
                        ));
                    } else {
                        self.diagnostics.push(Diagnostic::new(
                            access.syntax.span.clone(),
                            format!("module `{}` has no public value named `{item}`", namespace),
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
            Expression::Logical(logical) => {
                self.resolve_type(&logical.bool_type);
                self.resolve_expression(&logical.left, None, None);
                self.resolve_expression(&logical.right, None, None);
            }
            Expression::Name(name) if name.name == "_" => {}
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
                        && self.symbol_owners.get(&symbol).is_some_and(Option::is_some) =>
                {
                    self.symbols.insert(name.syntax.id, symbol);
                    self.record_capture(symbol);
                }
                (Some(symbol), methods)
                    if !methods.is_empty()
                        && self.intrinsic_functions.get(&symbol)
                            != Some(&IntrinsicFunction::Drop)
                        && !methods.iter().all(|method| {
                            self.trait_method_traits
                                .get(method)
                                .is_some_and(|trait_id| {
                                    ["Index", "MutateIndex"].iter().any(|name| {
                                        self.prelude_traits.get(*name) == Some(trait_id)
                                    })
                                })
                        }) =>
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
                (None, _) if self.lookup_namespace(&name.name).is_some() => {
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
            Expression::StringTemplate(template) => {
                for part in &template.parts {
                    if let staple_syntax::StringTemplatePart::Interpolation(interpolation) = part {
                        self.resolve_expression(&interpolation.expression, None, None);
                    }
                }
            }
            Expression::Quote(quote) => self.diagnostics.push(Diagnostic::new(
                quote.syntax.span.clone(),
                format!(
                    "`{}` is only available during macro expansion",
                    quote.kind.name()
                ),
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
            Expression::Binary(_) => unreachable!("binary expression was not desugared"),
        }
    }

    fn resolve_type(&mut self, ty: &Type) {
        self.resolve_type_with(ty, true);
    }

    fn resolve_type_lenient(&mut self, ty: &Type) {
        self.resolve_type_with(ty, false);
    }

    fn resolve_type_with(&mut self, ty: &Type, strict: bool) {
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
                        .or_else(|| self.lookup_namespace(namespace))
                        .and_then(|module| self.qualified_interface(module).types.get(&named.name))
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
                            self.block_types
                                .iter()
                                .rev()
                                .find_map(|frame| frame.get(&named.name).copied())
                        })
                        .or_else(|| {
                            self.declared_types[self.current_module.0]
                                .get(&named.name)
                                .copied()
                        })
                        .or_else(|| {
                            self.imported_types
                                .iter()
                                .rev()
                                .find_map(|frame| frame.get(&named.name).copied())
                        })
                        .or_else(|| self.prelude_types.get(&named.name).copied())
                };
                if let Some(id) = resolved {
                    self.named_types.insert(named.syntax.id, id);
                } else if strict && named.name != "int" {
                    let message =
                        unknown_item_message("type", &named.name, &self.private_glob_types);
                    self.diagnostics
                        .push(Diagnostic::new(named.syntax.span.clone(), message));
                }
            }
            Type::Product(product) => {
                for element in &product.elements {
                    self.resolve_type_with(&element.ty, strict);
                    if let Some(default) = &element.default {
                        self.resolve_expression(default, Some(&element.ty), None);
                    }
                }
            }
            Type::Sum(sum) => {
                for alternative in &sum.alternatives {
                    self.resolve_type_with(alternative, strict);
                }
            }
            Type::Function(function) => {
                self.resolve_type_with(&function.parameter, strict);
                for resource in &function.effects.resources {
                    self.resolve_type_with(&resource.value_type, strict);
                }
                self.resolve_type_with(&function.result, strict);
            }
            Type::Application(application) => {
                self.resolve_type_with(&application.callee, strict);
                self.resolve_type_with(&application.argument, strict);
            }
            Type::Repeated(repeated) => {
                self.resolve_type_with(&repeated.element, strict);
                if let Some(count) = &repeated.count {
                    self.resolve_type_with(count, strict);
                }
            }
            Type::Splice(splice) => {
                if strict {
                    self.diagnostics.push(Diagnostic::new(
                        splice.syntax.span.clone(),
                        "type splices are only available during macro expansion",
                    ));
                }
            }
            Type::Inferred(_) | Type::NumberLiteral(_) | Type::StringLiteral(_) => {}
        }
    }

    fn resolve_trait_name(&mut self, name: &staple_syntax::NamedType) -> Option<TraitId> {
        let resolved = if let Some(namespace) = &name.namespace {
            self.lookup_namespace(namespace)
                .as_ref()
                .and_then(|module| self.qualified_interface(*module).traits.get(&name.name))
                .copied()
        } else {
            self.declared_traits[self.current_module.0]
                .get(&name.name)
                .copied()
                .or_else(|| {
                    self.imported_traits
                        .iter()
                        .rev()
                        .find_map(|frame| frame.get(&name.name).copied())
                })
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
                .or_else(|| {
                    self.imported_traits
                        .iter()
                        .rev()
                        .find_map(|frame| frame.get(&name.name).copied())
                })
                .or_else(|| self.prelude_traits.get(&name.name).copied()),
            Expression::Access(access) => {
                let (namespace, name, _, _) = qualified_access_path(access)?;
                self.lookup_namespace(&namespace)
                    .as_ref()
                    .and_then(|module| self.qualified_interface(*module).traits.get(&name))
                    .copied()
            }
            _ => None,
        }
    }

    fn resolve_pattern_types(&mut self, pattern: &Pattern) {
        self.resolve_pattern_types_with(pattern, true);
    }

    fn resolve_pattern_types_lenient(&mut self, pattern: &Pattern) {
        self.resolve_pattern_types_with(pattern, false);
    }

    fn resolve_pattern_types_with(&mut self, pattern: &Pattern, strict: bool) {
        match pattern {
            Pattern::Wildcard(wildcard) => self.resolve_type_with(&wildcard.ty, strict),
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
                    self.resolve_type_with(&binding.ty, strict);
                }
            }
            Pattern::At(at) => {
                self.resolve_type_with(&at.binding.ty, strict);
                self.resolve_pattern_types_with(&at.pattern, strict);
            }
            Pattern::Product(product) => {
                for element in &product.elements {
                    self.resolve_pattern_types_with(element, strict);
                }
            }
            Pattern::Nominal(pattern) => {
                self.resolve_type_with(
                    &Type::Named(staple_syntax::NamedType {
                        syntax: pattern.syntax.clone(),
                        namespace: pattern.namespace.clone(),
                        name: pattern.name.clone(),
                    }),
                    strict,
                );
                self.resolve_pattern_types_with(&pattern.argument, strict);
            }
            Pattern::Splice(splice) => {
                if strict {
                    self.diagnostics.push(Diagnostic::new(
                        splice.syntax.span.clone(),
                        "pattern splices are only available during macro expansion",
                    ));
                }
            }
        }
    }

    fn validate_representation(&mut self, ty: &Type, required: Visibility) {
        match ty {
            Type::Named(named) => {
                if self.type_parameters.contains_key(&named.syntax.id) {
                    return;
                }
                if let Some(id) = self.named_types.get(&named.syntax.id).copied()
                    && !self.type_declarations[&id].visibility.meets(required)
                {
                    self.diagnostics.push(Diagnostic::new(
                        named.syntax.span.clone(),
                        if required == Visibility::Public {
                            format!(
                                "public representation references private type `{}`",
                                named.name
                            )
                        } else {
                            format!(
                                "package representation references private type `{}`",
                                named.name
                            )
                        },
                    ));
                }
            }
            Type::Product(product) => {
                for element in &product.elements {
                    self.validate_representation(&element.ty, required);
                }
            }
            Type::Sum(sum) => {
                for alternative in &sum.alternatives {
                    self.validate_representation(alternative, required);
                }
            }
            Type::Function(function) => {
                self.validate_representation(&function.parameter, required);
                for resource in &function.effects.resources {
                    self.validate_representation(&resource.value_type, required);
                }
                self.validate_representation(&function.result, required);
            }
            Type::Application(application) => {
                self.validate_representation(&application.callee, required);
                self.validate_representation(&application.argument, required);
            }
            Type::Repeated(repeated) => {
                self.validate_representation(&repeated.element, required);
                if let Some(count) = &repeated.count {
                    self.validate_representation(count, required);
                }
            }
            Type::Inferred(_) | Type::NumberLiteral(_) | Type::StringLiteral(_) | Type::Splice(_) => {}
        }
    }

    fn resolve_block(&mut self, block: &BlockExpression) {
        self.push_scope();
        self.namespaces.push(HashMap::new());
        self.block_types.push(HashMap::new());
        self.imported_types.push(HashMap::new());
        self.imported_macros.push(HashMap::new());
        self.imported_traits.push(HashMap::new());
        for item in &block.items {
            match item {
                Item::Binding(binding)
                    if matches!(binding.kind, BindingKind::Def | BindingKind::Const) =>
                {
                    self.declare_fresh(binding, None);
                }
                Item::Submodule(submodule) => self.declare_block_namespace(submodule),
                Item::TypeDeclaration(declaration) => self.declare_block_type(declaration),
                Item::UseDeclaration(declaration) => self.install_import(declaration),
                _ => {}
            }
        }
        for item in &block.items {
            self.resolve_block_item(item);
        }
        self.imported_traits.pop();
        self.imported_macros.pop();
        self.imported_types.pop();
        self.block_types.pop();
        self.namespaces.pop();
        self.pop_scope();
    }

    fn declare_block_namespace(&mut self, submodule: &Submodule) {
        let Some(&module) = self.submodule_ids.get(&submodule.syntax.id) else {
            return;
        };
        if self.current_scope().contains_key(&submodule.name)
            || self
                .namespaces
                .last()
                .expect("resolver namespace scope")
                .contains_key(&submodule.name)
        {
            self.diagnostics.push(Diagnostic::new(
                submodule.syntax.span.clone(),
                format!("duplicate definition of `{}`", submodule.name),
            ));
            return;
        }
        self.namespaces
            .last_mut()
            .expect("resolver namespace scope")
            .insert(submodule.name.clone(), module);
    }

    fn declare_block_type(&mut self, declaration: &TypeDeclaration) {
        let Some(&id) = self.type_ids_by_syntax.get(&declaration.syntax.id) else {
            return;
        };
        if self
            .block_types
            .last()
            .expect("resolver type scope")
            .contains_key(&declaration.name)
            || self
                .imported_types
                .last()
                .is_some_and(|frame| frame.contains_key(&declaration.name))
        {
            self.diagnostics.push(Diagnostic::new(
                declaration.syntax.span.clone(),
                format!("duplicate type definition of `{}`", declaration.name),
            ));
            return;
        }
        self.block_types
            .last_mut()
            .expect("resolver type scope")
            .insert(declaration.name.clone(), id);
        if let Some(&symbol) = self.type_constructor_symbols.get(&id) {
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

    fn declare_pattern(
        &mut self,
        pattern: &Pattern,
        shadow: Option<bool>,
        seen: &mut HashSet<String>,
    ) {
        match pattern {
            Pattern::Wildcard(_) | Pattern::StringLiteral(_) | Pattern::Splice(_) => {}
            Pattern::Binding(binding) => {
                if self.nominal_patterns.contains_key(&binding.syntax.id) {
                    return;
                }
                if !seen.insert(binding.name.clone()) {
                    self.diagnostics.push(Diagnostic::new(
                        binding.syntax.span.clone(),
                        format!(
                            "`{}` is bound more than once in the same pattern",
                            binding.name
                        ),
                    ));
                    return;
                }
                if let Some(symbol) = self.declared_symbols.get(&binding.syntax.id).copied() {
                    self.declare_symbol(
                        &binding.name,
                        binding.syntax.id,
                        binding.syntax.span.clone(),
                        symbol,
                        shadow,
                    );
                } else {
                    self.declare_fresh_name(
                        &binding.name,
                        binding.syntax.id,
                        binding.syntax.span.clone(),
                        shadow,
                    );
                }
                if let Some(symbol) = self.symbols.get(&binding.syntax.id).copied() {
                    if binding.mutable {
                        self.mutable_symbols.insert(symbol);
                        self.mutable_annotations.insert(symbol);
                    }
                }
            }
            Pattern::At(at) => {
                let binding = &at.binding;
                if !seen.insert(binding.name.clone()) {
                    self.diagnostics.push(Diagnostic::new(
                        binding.syntax.span.clone(),
                        format!(
                            "`{}` is bound more than once in the same pattern",
                            binding.name
                        ),
                    ));
                    return;
                }
                if let Some(symbol) = self.declared_symbols.get(&binding.syntax.id).copied() {
                    self.declare_symbol(
                        &binding.name,
                        binding.syntax.id,
                        binding.syntax.span.clone(),
                        symbol,
                        shadow,
                    );
                } else {
                    self.declare_fresh_name(
                        &binding.name,
                        binding.syntax.id,
                        binding.syntax.span.clone(),
                        shadow,
                    );
                }
                if let Some(symbol) = self.symbols.get(&binding.syntax.id).copied() {
                    if binding.mutable {
                        self.mutable_symbols.insert(symbol);
                        self.mutable_annotations.insert(symbol);
                    }
                }
                self.declare_pattern(&at.pattern, shadow, seen);
            }
            Pattern::Product(product) => {
                for element in &product.elements {
                    self.declare_pattern(element, shadow, seen);
                }
            }
            Pattern::Nominal(pattern) => {
                if let Some(id) = self.named_types.get(&pattern.syntax.id).copied() {
                    let declaration = &self.type_declarations[&id];
                    let represented = (declaration.kind == staple_syntax::TypeDeclarationKind::Distinct
                        && declaration.underlying.is_some())
                        || declaration.kind == staple_syntax::TypeDeclarationKind::Singleton;
                    if !represented {
                        self.diagnostics.push(Diagnostic::new(
                            pattern.syntax.span.clone(),
                            format!("`{}` is not a represented nominal type", pattern.name),
                        ));
                    } else if !self.representation_visible(id, self.current_module) {
                        self.diagnostics.push(Diagnostic::new(
                            pattern.syntax.span.clone(),
                            format!("the representation of `{}` is private", pattern.name),
                        ));
                    } else {
                        self.nominal_patterns.insert(pattern.syntax.id, id);
                    }
                }
                self.declare_pattern(&pattern.argument, shadow, seen);
            }
        }
    }

    fn declare_allocated(&mut self, binding: &Binding, shadow: Option<bool>) {
        if let Some(symbol) = self.declared_symbols.get(&binding.syntax.id).copied() {
            self.declare_symbol(
                &binding.name,
                binding.syntax.id,
                binding.syntax.span.clone(),
                symbol,
                shadow,
            );
        } else {
            self.declare_fresh(binding, shadow);
        }
        if let Some(symbol) = self.symbols.get(&binding.syntax.id).copied() {
            if binding.mutable {
                self.mutable_symbols.insert(symbol);
                self.mutable_annotations.insert(symbol);
            }
            if binding.signal {
                self.signal_symbols.insert(symbol);
            }
        }
    }

    fn declare_fresh(&mut self, binding: &Binding, shadow: Option<bool>) {
        self.declare_fresh_name(
            &binding.name,
            binding.syntax.id,
            binding.syntax.span.clone(),
            shadow,
        );
    }

    fn declare_fresh_name(
        &mut self,
        name: &str,
        syntax: SyntaxId,
        span: Span,
        shadow: Option<bool>,
    ) {
        let symbol = SymbolId(self.next_symbol_id);
        self.next_symbol_id += 1;
        self.symbol_owners
            .insert(symbol, self.function_stack.last().copied());
        self.symbol_modules.insert(symbol, self.current_module);
        self.symbol_declarations.insert(symbol, syntax);
        self.declare_symbol(name, syntax, span, symbol, shadow);
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

    fn declare_symbol(
        &mut self,
        name: &str,
        syntax: SyntaxId,
        span: Span,
        symbol: SymbolId,
        shadow: Option<bool>,
    ) {
        if self.namespaces.iter().any(|frame| frame.contains_key(name)) {
            self.diagnostics.push(Diagnostic::new(
                span,
                format!("duplicate definition of `{name}`"),
            ));
            return;
        }
        if self.current_scope().contains_key(name) {
            let can_shadow = match shadow {
                Some(new_is_pub) => self
                    .current_shadowable()
                    .get(name)
                    .is_some_and(|&existing_is_pub| !(existing_is_pub && new_is_pub)),
                None => false,
            };
            if !can_shadow {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    format!("duplicate definition of `{name}`"),
                ));
                return;
            }
        }
        self.current_scope_mut().insert(name.to_owned(), symbol);
        match shadow {
            Some(is_pub) => {
                self.current_shadowable_mut()
                    .insert(name.to_owned(), is_pub);
            }
            None => {
                self.current_shadowable_mut().remove(name);
            }
        }
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

    fn lookup_namespace(&self, name: &str) -> Option<ModuleId> {
        self.namespaces
            .iter()
            .rev()
            .find_map(|frame| frame.get(name).copied())
            .or_else(|| self.prelude_namespaces.get(name).copied())
    }

    fn lookup_macro(&self, name: &str) -> Option<Vec<MacroId>> {
        if self.lookup(name).is_some() {
            return None;
        }
        self.declared_macros[self.current_module.0]
            .get(name)
            .cloned()
            .or_else(|| {
                self.imported_macros
                    .iter()
                    .rev()
                    .find_map(|frame| frame.get(name).cloned())
            })
            .or_else(|| self.prelude_macros.get(name).cloned())
    }

    fn resolve_primitive_macro(&self, callee: &Expression) -> Option<PrimitiveMacro> {
        let ids = match callee {
            Expression::Name(name) => self.lookup_macro(&name.name),
            Expression::Access(access) => {
                let (namespace, item, definition_module, _) = qualified_access_path(access)?;
                definition_module
                    .and_then(|context| {
                        self.definition_context_namespaces
                            .get(context)
                            .and_then(|namespaces| namespaces.get(&namespace))
                            .copied()
                    })
                    .or_else(|| self.lookup_namespace(&namespace))
                    .and_then(|module| self.qualified_interface(module).macros.get(&item))
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
        self.shadowable.push(HashMap::new());
    }
    fn pop_scope(&mut self) {
        self.scopes.pop();
        self.shadowable.pop();
    }
    fn current_scope(&self) -> &HashMap<String, SymbolId> {
        self.scopes.last().expect("resolver scope")
    }
    fn current_scope_mut(&mut self) -> &mut HashMap<String, SymbolId> {
        self.scopes.last_mut().expect("resolver scope")
    }
    fn current_shadowable(&self) -> &HashMap<String, bool> {
        self.shadowable.last().expect("resolver scope")
    }
    fn current_shadowable_mut(&mut self) -> &mut HashMap<String, bool> {
        self.shadowable.last_mut().expect("resolver scope")
    }
}

fn mutable_pattern_symbols(resolver: &NameResolver, pattern: &Pattern) -> Vec<SymbolId> {
    let mut symbols = Vec::new();
    fn collect(resolver: &NameResolver, pattern: &Pattern, symbols: &mut Vec<SymbolId>) {
        match pattern {
            Pattern::Binding(binding) => {
                if binding.mutable
                    && let Some(symbol) = resolver.symbols.get(&binding.syntax.id).copied()
                {
                    symbols.push(symbol);
                }
            }
            Pattern::At(at) => {
                if at.binding.mutable
                    && let Some(symbol) = resolver.symbols.get(&at.binding.syntax.id).copied()
                {
                    symbols.push(symbol);
                }
                collect(resolver, &at.pattern, symbols);
            }
            Pattern::Product(product) => {
                for element in &product.elements {
                    collect(resolver, element, symbols);
                }
            }
            Pattern::Nominal(nominal) => collect(resolver, &nominal.argument, symbols),
            Pattern::Wildcard(_) | Pattern::StringLiteral(_) | Pattern::Splice(_) => {}
        }
    }
    collect(resolver, pattern, &mut symbols);
    symbols
}

fn pattern_contains_mutable_marker(pattern: &Pattern) -> bool {
    match pattern {
        Pattern::Binding(binding) => binding.mutable,
        Pattern::At(at) => at.binding.mutable || pattern_contains_mutable_marker(&at.pattern),
        Pattern::Product(product) => product.elements.iter().any(pattern_contains_mutable_marker),
        Pattern::Nominal(nominal) => pattern_contains_mutable_marker(&nominal.argument),
        Pattern::Wildcard(_) | Pattern::StringLiteral(_) | Pattern::Splice(_) => false,
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

#[derive(Clone)]
struct CompileTimeScope {
    frames: Vec<HashMap<String, CompileTimeBindingInfo>>,
    occurrences: HashMap<SyntaxId, CompileTimeBindingInfo>,
    module: ModuleId,
}

impl CompileTimeScope {
    fn new(module: ModuleId, globals: HashMap<String, CompileTimeBindingInfo>) -> Self {
        Self {
            frames: vec![globals],
            occurrences: HashMap::new(),
            module,
        }
    }

    fn declare(
        &mut self,
        syntax: SyntaxId,
        name: String,
        type_display: Option<String>,
        kind: CompileTimeBindingKind,
        mutable: bool,
        declaration_prefix: Option<String>,
    ) {
        let info = CompileTimeBindingInfo {
            declaration: syntax,
            name: name.clone(),
            type_display,
            kind,
            mutable,
            declaration_prefix,
            module: self.module,
            definition: None,
        };
        self.frames.last_mut().unwrap().insert(name, info.clone());
        self.occurrences.insert(syntax, info);
    }

    fn reference(&mut self, syntax: SyntaxId, name: &str) {
        if let Some(info) = self
            .frames
            .iter()
            .rev()
            .find_map(|frame| frame.get(name))
            .cloned()
        {
            self.occurrences.insert(syntax, info);
        }
    }

    fn lookup_type(&self, name: &str) -> Option<String> {
        self.frames
            .iter()
            .rev()
            .find_map(|frame| frame.get(name))
            .and_then(|info| info.type_display.clone())
    }
}

fn analyze_compile_time_bindings(
    program: &Program,
    helpers: &[(ModuleId, Binding)],
) -> HashMap<SyntaxId, CompileTimeBindingInfo> {
    let mut globals = HashMap::new();
    for (module, helper) in helpers {
        let type_display = helper.annotation.as_ref().map(ToString::to_string);
        globals.insert(
            helper.name.clone(),
            CompileTimeBindingInfo {
                declaration: helper.syntax.id,
                name: helper.name.clone(),
                type_display,
                kind: CompileTimeBindingKind::Helper,
                mutable: false,
                declaration_prefix: Some("def".to_owned()),
                module: *module,
                definition: None,
            },
        );
    }
    let mut result = HashMap::new();
    for (module, helper) in helpers {
        let mut scope = CompileTimeScope::new(*module, globals.clone());
        scope.reference(helper.syntax.id, &helper.name);
        if let Some(value) = &helper.value {
            analyze_compile_helper_expression(value, helper.annotation.as_ref(), &mut scope);
        }
        result.extend(scope.occurrences);
    }
    for source_module in program.modules() {
        for item in &source_module.syntax.items {
            if let Item::MacroDeclaration(declaration) = item
                && let Some(value) = &declaration.value
            {
                let mut scope = CompileTimeScope::new(source_module.id, globals.clone());
                analyze_compile_expression(
                    value,
                    &mut scope,
                    CompileTimeBindingKind::MacroParameter,
                    false,
                );
                result.extend(scope.occurrences);
            }
        }
    }
    result
}

fn compile_pattern_type(pattern: &Pattern, fallback: Option<String>) -> Option<String> {
    match pattern {
        Pattern::At(at) if matches!(at.binding.ty, Type::Inferred(_)) => fallback.or_else(|| {
            crate::macro_expand::pattern_meta_type(pattern)
                .map(|ty| crate::macro_expand::format_meta_type(&ty))
        }),
        Pattern::Binding(binding) if matches!(binding.ty, Type::Inferred(_)) => {
            fallback.or_else(|| {
                crate::macro_expand::pattern_meta_type(pattern)
                    .map(|ty| crate::macro_expand::format_meta_type(&ty))
            })
        }
        Pattern::Wildcard(wildcard) if matches!(wildcard.ty, Type::Inferred(_)) => fallback
            .or_else(|| {
                crate::macro_expand::pattern_meta_type(pattern)
                    .map(|ty| crate::macro_expand::format_meta_type(&ty))
            }),
        _ => crate::macro_expand::pattern_meta_type(pattern)
            .map(|ty| crate::macro_expand::format_meta_type(&ty))
            .or(fallback),
    }
}

fn analyze_compile_helper_expression(
    expression: &Expression,
    annotation: Option<&Type>,
    scope: &mut CompileTimeScope,
) {
    if let (Expression::Function(function), Some(Type::Function(function_type))) =
        (expression, annotation)
    {
        scope.frames.push(HashMap::new());
        declare_compile_pattern(
            &function.pattern,
            scope,
            CompileTimeBindingKind::HelperParameter,
            Some(function_type.parameter.to_string()),
        );
        analyze_compile_helper_expression(&function.body, Some(&function_type.result), scope);
        scope.frames.pop();
    } else {
        analyze_compile_expression(
            expression,
            scope,
            CompileTimeBindingKind::HelperParameter,
            false,
        );
    }
}

fn compile_time_builtin_signature(name: &str) -> Option<&str> {
    match name {
        "Ident" => Some("String -> Ident String"),
        "CallExpr" => Some("(callee: Expr, argument: Expr) -> CallExpr"),
        "StringExpr" => Some("String -> StringExpr"),
        "BindingPattern" => Some("Ident String -> BindingPattern"),
        "NominalPattern" => Some("(name: Ident String, argument: Pattern) -> NominalPattern"),
        "Sequence" => Some("Element => Element -> Sequence Element"),
        "Separated" => Some(
            "Element => Separator => (separator: Separator, elements: Sequence Element, trailing: Bool) -> Separated Element Separator",
        ),
        "Parenthesized" => Some("Contents => Contents -> Parenthesized Contents"),
        "Bracketed" => Some("Contents => Contents -> Bracketed Contents"),
        "Braced" => Some("Contents => Contents -> Braced Contents"),
        "TypeDeclarationItem" => Some(
            "(kind: TypeDeclarationKind, name: Ident String, name_spelling: String, declared_type: Type, type_parameters: Sequence (Ident String), underlying: Optional Type) -> TypeDeclarationItem",
        ),
        "ModifiedItem" => Some("(modifiers: Sequence Modifier, item: Item) -> ModifiedItem"),
        "Syntax"
        | "SyntaxNode"
        | "Expr"
        | "UnstructuredExpr"
        | "Type"
        | "Pattern"
        | "Item"
        | "UnstructuredItem"
        | "Modifier"
        | "TypeDeclarationKind"
        | "AliasDeclaration"
        | "DistinctDeclaration"
        | "SingletonDeclaration"
        | "OpaqueDeclaration"
        | "Visibility"
        | "MacroCallMetadata"
        | "Comma"
        | "Equals"
        | "FatArrow" => Some(name),
        _ => None,
    }
}

fn declare_compile_pattern(
    pattern: &Pattern,
    scope: &mut CompileTimeScope,
    kind: CompileTimeBindingKind,
    fallback: Option<String>,
) {
    match pattern {
        Pattern::At(at) => {
            let ty = compile_pattern_type(pattern, fallback.clone());
            scope.declare(
                at.binding.syntax.id,
                at.binding.name.clone(),
                ty,
                kind,
                at.binding.mutable,
                None,
            );
            declare_compile_pattern(&at.pattern, scope, CompileTimeBindingKind::Local, fallback);
        }
        Pattern::Binding(binding) => {
            let ty = compile_pattern_type(pattern, fallback);
            scope.declare(
                binding.syntax.id,
                binding.name.clone(),
                ty,
                kind,
                binding.mutable,
                None,
            );
        }
        Pattern::Product(product) => {
            for element in &product.elements {
                declare_compile_pattern(element, scope, kind, None);
            }
        }
        Pattern::Nominal(nominal) => declare_compile_pattern(&nominal.argument, scope, kind, None),
        Pattern::Wildcard(_) | Pattern::StringLiteral(_) | Pattern::Splice(_) => {}
    }
}

fn compile_expression_type(expression: &Expression, scope: &CompileTimeScope) -> Option<String> {
    match expression {
        Expression::Name(name) => scope.lookup_type(&name.name).or_else(|| {
            matches!(
                name.name.as_str(),
                "Syntax"
                    | "SyntaxNode"
                    | "Expr"
                    | "Ident"
                    | "CallExpr"
                    | "StringExpr"
                    | "UnstructuredExpr"
                    | "Type"
                    | "Pattern"
                    | "BindingPattern"
                    | "NominalPattern"
                    | "Item"
                    | "Modifier"
                    | "ModifiedItem"
                    | "TypeDeclarationItem"
                    | "UnstructuredItem"
                    | "TypeDeclarationKind"
                    | "AliasDeclaration"
                    | "DistinctDeclaration"
                    | "SingletonDeclaration"
                    | "OpaqueDeclaration"
                    | "Visibility"
                    | "MacroCallMetadata"
                    | "Comma"
                    | "Equals"
                    | "FatArrow"
            )
            .then(|| name.name.clone())
        }),
        Expression::Quote(quote) => Some(match quote.kind {
            staple_syntax::QuoteKind::Quote => "Syntax".to_owned(),
            staple_syntax::QuoteKind::ParseQuote => "SyntaxNode".to_owned(),
        }),
        Expression::String(_) => Some("String".to_owned()),
        Expression::Integer(_) => Some("Integer".to_owned()),
        Expression::Float(_) => Some("Float".to_owned()),
        Expression::Satisfies(value) => Some(value.ty.to_string()),
        Expression::Product(product) => Some(format!(
            "({})",
            product
                .elements
                .iter()
                .map(|element| compile_expression_type(&element.value, scope)
                    .unwrap_or_else(|| "?".to_owned()))
                .collect::<Vec<_>>()
                .join(", ")
        )),
        Expression::Call(call) => {
            let callee = compile_expression_type(&call.callee, scope)?;
            callee
                .split_once(" -> ")
                .map(|(_, result)| result.to_owned())
        }
        Expression::Block(block) => block.items.last().and_then(|item| match item {
            Item::Expression(value) => compile_expression_type(value, scope),
            Item::Return(value) => compile_expression_type(&value.value, scope),
            _ => None,
        }),
        Expression::Match(value) => value
            .arms
            .first()
            .and_then(|arm| compile_expression_type(&arm.body, scope)),
        _ => None,
    }
}

fn analyze_compile_expression(
    expression: &Expression,
    scope: &mut CompileTimeScope,
    parameter_kind: CompileTimeBindingKind,
    quoted: bool,
) {
    match expression {
        Expression::Function(function) => {
            scope.frames.push(HashMap::new());
            declare_compile_pattern(&function.pattern, scope, parameter_kind, None);
            analyze_compile_expression(&function.body, scope, parameter_kind, quoted);
            scope.frames.pop();
        }
        Expression::Match(value) => {
            analyze_compile_expression(&value.subject, scope, parameter_kind, quoted);
            let subject_type = compile_expression_type(&value.subject, scope);
            for arm in &value.arms {
                scope.frames.push(HashMap::new());
                declare_compile_pattern(
                    &arm.pattern,
                    scope,
                    CompileTimeBindingKind::Local,
                    subject_type.clone(),
                );
                analyze_compile_expression(&arm.body, scope, parameter_kind, quoted);
                scope.frames.pop();
            }
        }
        Expression::Block(block) => {
            scope.frames.push(HashMap::new());
            for item in &block.items {
                analyze_compile_item(item, scope, parameter_kind, quoted);
            }
            scope.frames.pop();
        }
        Expression::Quote(quote) => match &quote.template {
            staple_syntax::QuoteTemplate::Expression(value) => {
                analyze_compile_expression(value, scope, parameter_kind, true)
            }
            staple_syntax::QuoteTemplate::Item(item) => {
                analyze_compile_item(item, scope, parameter_kind, true)
            }
            staple_syntax::QuoteTemplate::Items(items) => {
                for item in items {
                    analyze_compile_item(item, scope, parameter_kind, true);
                }
            }
            staple_syntax::QuoteTemplate::Raw => {}
        },
        Expression::Splice(value) => scope.reference(value.syntax.id, &value.name),
        Expression::Name(value) if !quoted => {
            scope.reference(value.syntax.id, &value.name);
            if !scope.occurrences.contains_key(&value.syntax.id)
                && compile_time_builtin_signature(&value.name).is_some()
            {
                scope.occurrences.insert(
                    value.syntax.id,
                    CompileTimeBindingInfo {
                        declaration: value.syntax.id,
                        name: value.name.clone(),
                        type_display: compile_time_builtin_signature(&value.name)
                            .map(str::to_owned),
                        kind: CompileTimeBindingKind::Builtin,
                        mutable: false,
                        declaration_prefix: None,
                        module: scope.module,
                        definition: None,
                    },
                );
            }
        }
        Expression::Satisfies(value) => {
            analyze_compile_expression(&value.value, scope, parameter_kind, quoted)
        }
        Expression::Product(value) => {
            for element in &value.elements {
                analyze_compile_expression(&element.value, scope, parameter_kind, quoted);
            }
        }
        Expression::RepeatedProduct(value) => {
            analyze_compile_expression(&value.value, scope, parameter_kind, quoted);
            analyze_compile_expression(&value.count, scope, parameter_kind, quoted);
        }
        Expression::Call(value) => {
            analyze_compile_expression(&value.callee, scope, parameter_kind, quoted);
            analyze_compile_expression(&value.argument, scope, parameter_kind, quoted);
        }
        Expression::Access(value) => {
            analyze_compile_expression(&value.value, scope, parameter_kind, quoted)
        }
        Expression::Index(value) => {
            analyze_compile_expression(&value.value, scope, parameter_kind, quoted);
            analyze_compile_expression(&value.index, scope, parameter_kind, quoted);
        }
        Expression::Loop(value) => {
            for item in &value.body.items {
                analyze_compile_item(item, scope, parameter_kind, quoted);
            }
        }
        Expression::With(value) => {
            analyze_compile_expression(&value.value, scope, parameter_kind, quoted);
            for item in &value.body.items {
                analyze_compile_item(item, scope, parameter_kind, quoted);
            }
        }
        _ => {}
    }
}

fn analyze_compile_item(
    item: &Item,
    scope: &mut CompileTimeScope,
    parameter_kind: CompileTimeBindingKind,
    quoted: bool,
) {
    if quoted {
        match item {
            Item::Expression(value) => {
                analyze_compile_expression(value, scope, parameter_kind, true)
            }
            Item::Binding(binding) => {
                if let Some(value) = &binding.value {
                    analyze_compile_expression(value, scope, parameter_kind, true);
                }
            }
            Item::PatternBinding(binding) => {
                analyze_compile_expression(&binding.value, scope, parameter_kind, true)
            }
            _ => {}
        }
        return;
    }
    match item {
        Item::Binding(binding) => {
            if let Some(value) = &binding.value {
                analyze_compile_expression(value, scope, parameter_kind, false);
            }
            let ty = binding
                .annotation
                .as_ref()
                .map(ToString::to_string)
                .or_else(|| {
                    binding
                        .value
                        .as_ref()
                        .and_then(|value| compile_expression_type(value, scope))
                });
            let mut prefix = binding.keyword().to_owned();
            if binding.mutable {
                prefix.push_str(" mut");
            }
            scope.declare(
                binding.syntax.id,
                binding.name.clone(),
                ty,
                CompileTimeBindingKind::Local,
                binding.mutable,
                Some(prefix),
            );
        }
        Item::PatternBinding(binding) => {
            analyze_compile_expression(&binding.value, scope, parameter_kind, false);
            let ty = compile_expression_type(&binding.value, scope);
            declare_compile_pattern(&binding.pattern, scope, CompileTimeBindingKind::Local, ty);
        }
        Item::Assignment(value) => {
            analyze_compile_expression(&value.target, scope, parameter_kind, false);
            analyze_compile_expression(&value.value, scope, parameter_kind, false);
        }
        Item::Expression(value) => analyze_compile_expression(value, scope, parameter_kind, false),
        Item::Return(value) => {
            analyze_compile_expression(&value.value, scope, parameter_kind, false)
        }
        Item::Break(value) => {
            if let Some(value) = &value.value {
                analyze_compile_expression(value, scope, parameter_kind, false);
            }
        }
        _ => {}
    }
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
                match item {
                    Item::Binding(binding) => {
                        if let Some(symbol) = self.module.symbol_for(binding.syntax.id) {
                            globals.insert(symbol, InitializationState::Declared);
                        }
                    }
                    Item::PatternBinding(binding) => {
                        self.set_pattern_state(
                            &binding.pattern,
                            InitializationState::Declared,
                            &mut globals,
                        );
                    }
                    Item::Assignment(_)
                    | Item::Return(_)
                    | Item::Break(_)
                    | Item::Continue(_)
                    | Item::Expression(_)
                    | Item::Submodule(_)
                    | Item::TypeDeclaration(_)
                    | Item::UseDeclaration(_) => {}
                    _ => {}
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
                if matches!(
                    item,
                    Item::Binding(_)
                        | Item::PatternBinding(_)
                        | Item::Assignment(_)
                        | Item::Return(_)
                        | Item::Break(_)
                        | Item::Continue(_)
                        | Item::Expression(_)
                ) {
                    self.item(item, &mut globals, &HashMap::new(), true);
                }
            }
        }

        InitializationAnalysis {
            checked_symbols: self.checked_symbols,
            checked_reads: self.checked_reads,
            diagnostics: self.diagnostics,
        }
    }

    fn item(
        &mut self,
        item: &Item,
        local: &mut HashMap<SymbolId, InitializationState>,
        outer: &HashMap<SymbolId, InitializationState>,
        module_level: bool,
    ) {
        match item {
            Item::Binding(binding) => {
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
            Item::PatternBinding(binding) => {
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
            Item::Assignment(assignment) => {
                self.expression(&assignment.target, local, outer);
                self.expression(&assignment.value, local, outer);
            }
            Item::Return(item) => self.expression(&item.value, local, outer),
            Item::Break(item) => {
                if let Some(value) = &item.value {
                    self.expression(value, local, outer);
                }
            }
            Item::Continue(_) => {}
            Item::Expression(expression) => self.expression(expression, local, outer),
            Item::Submodule(_) => {}
            Item::TypeDeclaration(_) => {}
            Item::UseDeclaration(_) => {}
            _ => {}
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
                for item in &block.items {
                    if let Item::Binding(binding) = item
                        && binding.kind == BindingKind::Def
                        && let Some(symbol) = self.module.symbol_for(binding.syntax.id)
                    {
                        local.insert(symbol, InitializationState::Declared);
                    }
                }
                for item in &block.items {
                    self.item(item, local, outer, false);
                }
                *local = original;
            }
            Expression::Product(product) => {
                for element in &product.elements {
                    self.expression(&element.value, local, outer);
                }
            }
            Expression::RepeatedProduct(repeated) => {
                self.expression(&repeated.value, local, outer);
                self.expression(&repeated.count, local, outer);
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
            Expression::Logical(logical) => {
                self.expression(&logical.left, local, outer);
                self.expression(&logical.right, local, outer);
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
            | Expression::StringTemplate(_)
            | Expression::CString(_)
            | Expression::Integer(_)
            | Expression::Float(_) => {}
            Expression::Binary(_) => unreachable!("binary expression was not desugared"),
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
            Pattern::At(at) => {
                if let Some(symbol) = self.module.symbol_for(at.binding.syntax.id) {
                    states.insert(symbol, state);
                }
                self.set_pattern_state(&at.pattern, state, states);
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

/// Finds `type` declarations nested inside block expressions anywhere in
/// `items`, without descending into an `Item::Submodule`'s own body (that
/// submodule is a separate flat `SourceModule`, visited independently by
/// `collect_interfaces`'s own per-module loop).
fn find_block_type_declarations<'a>(items: &'a [Item], out: &mut Vec<&'a TypeDeclaration>) {
    for item in items {
        find_block_type_declarations_in_item(item, out);
    }
}

fn find_block_type_declarations_in_item<'a>(item: &'a Item, out: &mut Vec<&'a TypeDeclaration>) {
    match item {
        Item::Modified(modified) => find_block_type_declarations_in_item(&modified.item, out),
        Item::VisibilityMacroInvocation(invocation) => {
            find_block_type_declarations_in_expression(&invocation.expression, out)
        }
        Item::VisibilitySplice(splice) => find_block_type_declarations_in_item(&splice.item, out),
        Item::RepeatedItemSplice(_)
        | Item::UseDeclaration(_)
        | Item::Submodule(_)
        | Item::TypeDeclaration(_) => {}
        Item::ExternBlock(block) => {
            for binding in &block.bindings {
                if let Some(value) = &binding.value {
                    find_block_type_declarations_in_expression(value, out);
                }
            }
        }
        Item::MacroDeclaration(declaration) => {
            if let Some(value) = &declaration.value {
                find_block_type_declarations_in_expression(value, out);
            }
        }
        Item::TraitDeclaration(declaration) => {
            for member in &declaration.members {
                if let Some(default) = &member.default {
                    find_block_type_declarations_in_expression(default, out);
                }
            }
        }
        Item::TraitImplementation(implementation) => {
            for member in &implementation.members {
                find_block_type_declarations_in_expression(&member.value, out);
            }
        }
        item @ (Item::Binding(_)
        | Item::PatternBinding(_)
        | Item::Assignment(_)
        | Item::Return(_)
        | Item::Break(_)
        | Item::Continue(_)
        | Item::Expression(_)) => find_block_type_declarations_in_block_item(item, out),
    }
}

fn find_block_type_declarations_in_block_item<'a>(
    item: &'a Item,
    out: &mut Vec<&'a TypeDeclaration>,
) {
    match item {
        Item::Binding(binding) => {
            if let Some(value) = &binding.value {
                find_block_type_declarations_in_expression(value, out);
            }
        }
        Item::PatternBinding(binding) => {
            find_block_type_declarations_in_expression(&binding.value, out)
        }
        Item::Assignment(assignment) => {
            find_block_type_declarations_in_expression(&assignment.target, out);
            find_block_type_declarations_in_expression(&assignment.value, out);
        }
        Item::Return(item) => find_block_type_declarations_in_expression(&item.value, out),
        Item::Break(item) => {
            if let Some(value) = &item.value {
                find_block_type_declarations_in_expression(value, out);
            }
        }
        Item::Continue(_) => {}
        Item::Expression(expression) => find_block_type_declarations_in_expression(expression, out),
        Item::Submodule(_) => {}
        Item::TypeDeclaration(declaration) => out.push(declaration),
        Item::UseDeclaration(_) => {}
        _ => {}
    }
}

fn find_block_type_declarations_in_expression<'a>(
    expression: &'a Expression,
    out: &mut Vec<&'a TypeDeclaration>,
) {
    match expression {
        Expression::Function(function) => {
            find_block_type_declarations_in_expression(&function.body, out)
        }
        Expression::Satisfies(satisfies) => {
            find_block_type_declarations_in_expression(&satisfies.value, out)
        }
        Expression::Match(match_) => {
            find_block_type_declarations_in_expression(&match_.subject, out);
            for arm in &match_.arms {
                find_block_type_declarations_in_expression(&arm.body, out);
            }
        }
        Expression::Loop(loop_) => find_block_type_declarations_in_block(&loop_.body, out),
        Expression::Resource(_) => {}
        Expression::With(with) => {
            find_block_type_declarations_in_expression(&with.value, out);
            find_block_type_declarations_in_block(&with.body, out);
        }
        Expression::Block(block) => find_block_type_declarations_in_block(block, out),
        Expression::Product(product) => {
            for element in &product.elements {
                find_block_type_declarations_in_expression(&element.value, out);
            }
        }
        Expression::RepeatedProduct(repeated) => {
            find_block_type_declarations_in_expression(&repeated.value, out);
            find_block_type_declarations_in_expression(&repeated.count, out);
        }
        Expression::Call(call) => {
            find_block_type_declarations_in_expression(&call.callee, out);
            find_block_type_declarations_in_expression(&call.argument, out);
        }
        Expression::Access(access) => {
            find_block_type_declarations_in_expression(&access.value, out)
        }
        Expression::Index(index) => {
            find_block_type_declarations_in_expression(&index.value, out);
            find_block_type_declarations_in_expression(&index.index, out);
        }
        Expression::Logical(logical) => {
            find_block_type_declarations_in_expression(&logical.left, out);
            find_block_type_declarations_in_expression(&logical.right, out);
        }
        Expression::SyntaxArgument(_)
        | Expression::VisibilityArgument(_)
        | Expression::Quote(_)
        | Expression::Splice(_)
        | Expression::Name(_)
        | Expression::String(_)
        | Expression::StringTemplate(_)
        | Expression::CString(_)
        | Expression::Integer(_)
        | Expression::Float(_) => {}
        Expression::Binary(_) => unreachable!("binary expression was not desugared"),
    }
}

fn find_block_type_declarations_in_block<'a>(
    block: &'a BlockExpression,
    out: &mut Vec<&'a TypeDeclaration>,
) {
    for item in &block.items {
        find_block_type_declarations_in_block_item(item, out);
    }
}

/// Syntax ids of the `Name`/`Access` nodes making up the namespace portion
/// of a qualified access chain, in outermost-first order (e.g. for
/// `std.io.println`, the id of the `std` name node then the id of the
/// `std.io` access node).
type QualifiedAccessSegments = Vec<SyntaxId>;

fn qualified_access_path(
    access: &staple_syntax::AccessExpression,
) -> Option<(String, String, Option<usize>, QualifiedAccessSegments)> {
    fn collect(
        expression: &Expression,
        parts: &mut Vec<String>,
        segments: &mut QualifiedAccessSegments,
    ) -> Option<Option<usize>> {
        match expression {
            Expression::Name(name) => {
                parts.push(name.name.clone());
                segments.push(name.syntax.id);
                Some(name.syntax.definition_module())
            }
            Expression::Access(access) => {
                let definition_module = collect(&access.value, parts, segments)?;
                let Accessor::Name(name) = &access.accessor else {
                    return None;
                };
                parts.push(name.clone());
                segments.push(access.syntax.id);
                Some(definition_module)
            }
            _ => None,
        }
    }

    let mut parts = Vec::new();
    let mut segments = Vec::new();
    let definition_module = collect(&access.value, &mut parts, &mut segments)?;
    let Accessor::Name(item) = &access.accessor else {
        return None;
    };
    parts.push(item.clone());
    let item = parts.pop()?;
    (!parts.is_empty()).then(|| (parts.join("."), item, definition_module, segments))
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
