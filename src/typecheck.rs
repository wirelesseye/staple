use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::{
    Accessor, Binding, BuiltinType, Diagnostic, Expression, FunctionId, IntegerType, Item, Module,
    Pattern, PatternBindingKind, ProductType, ResolvedFunction, ResolvedModule, Span, Statement,
    SymbolId, SyntaxId, TraitId, TraitMethodId, Type, TypeDeclaration, TypeDeclarationKind, TypeId,
    TypeParameterId, TypeParameterPattern,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedTraitBound {
    pub trait_id: TraitId,
    pub argument: CheckedType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedTraitDispatch {
    pub method: TraitMethodId,
    pub target: CheckedType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedCoercion {
    pub source: CheckedType,
    pub target: CheckedType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedPropagation {
    pub source: CheckedType,
    pub success_index: usize,
    pub result: CheckedType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedMatch {
    pub source: CheckedType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedAccess {
    pub index: usize,
    pub dereference: Option<CheckedType>,
}

#[derive(Clone, Copy)]
enum CoveragePattern<'a> {
    Any,
    Pattern(&'a Pattern),
}

#[derive(Debug, Clone)]
struct ReturnContribution {
    syntax: Option<SyntaxId>,
    span: Span,
    value_type: CheckedType,
}

#[derive(Debug, Clone)]
struct CheckedTraitImplementation {
    trait_id: TraitId,
    target: CheckedType,
    methods: HashMap<TraitMethodId, FunctionId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckedType {
    Inferred,
    Error,
    I32,
    I8,
    I16,
    I64,
    U8,
    U16,
    U32,
    U64,
    ISize,
    USize,
    String,
    Ref(Box<CheckedType>),
    CString,
    CChar,
    Parameter {
        id: TypeParameterId,
        name: String,
    },
    TypeConstructor {
        id: TypeId,
        name: String,
        arguments: Vec<CheckedType>,
    },
    Opaque {
        id: TypeId,
        name: String,
        arguments: Vec<CheckedType>,
    },
    CPointer {
        pointee: Box<CheckedType>,
    },
    Product(CheckedProductType),
    Sum(CheckedSumType),
    Function(CheckedFunctionType),
    Distinct {
        id: TypeId,
        name: String,
        arguments: Vec<CheckedType>,
        representation: Box<CheckedType>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedProductType {
    pub elements: Vec<CheckedTypeElement>,
    pub variadic: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedTypeElement {
    pub name: Option<String>,
    pub value_type: CheckedType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedSumType {
    pub alternatives: Vec<CheckedType>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedFunctionType {
    pub parameter: Box<CheckedType>,
    pub result: Box<CheckedType>,
}

impl CheckedType {
    pub fn integer(integer: IntegerType) -> Self {
        match integer {
            IntegerType::I8 => Self::I8,
            IntegerType::I16 => Self::I16,
            IntegerType::I32 => Self::I32,
            IntegerType::I64 => Self::I64,
            IntegerType::U8 => Self::U8,
            IntegerType::U16 => Self::U16,
            IntegerType::U32 => Self::U32,
            IntegerType::U64 => Self::U64,
            IntegerType::ISize => Self::ISize,
            IntegerType::USize => Self::USize,
        }
    }

    pub fn integer_type(&self) -> Option<IntegerType> {
        Some(match self {
            Self::I8 => IntegerType::I8,
            Self::I16 => IntegerType::I16,
            Self::I32 => IntegerType::I32,
            Self::I64 => IntegerType::I64,
            Self::U8 => IntegerType::U8,
            Self::U16 => IntegerType::U16,
            Self::U32 => IntegerType::U32,
            Self::U64 => IntegerType::U64,
            Self::ISize => IntegerType::ISize,
            Self::USize => IntegerType::USize,
            _ => return None,
        })
    }

    pub fn empty_product() -> Self {
        Self::Product(CheckedProductType {
            elements: Vec::new(),
            variadic: false,
        })
    }

    pub fn is_concrete(&self) -> bool {
        match self {
            Self::Inferred | Self::Error | Self::TypeConstructor { .. } => false,
            Self::CPointer { pointee } => pointee.is_concrete(),
            Self::Ref(value) => value.is_concrete(),
            Self::Product(product) => product
                .elements
                .iter()
                .all(|element| element.value_type.is_concrete()),
            Self::Sum(sum) => sum.alternatives.iter().all(CheckedType::is_concrete),
            Self::Function(function) => {
                function.parameter.is_concrete() && function.result.is_concrete()
            }
            Self::Distinct { representation, .. } => representation.is_concrete(),
            Self::I32
            | Self::I8
            | Self::I16
            | Self::I64
            | Self::U8
            | Self::U16
            | Self::U32
            | Self::U64
            | Self::ISize
            | Self::USize
            | Self::String
            | Self::CString
            | Self::CChar
            | Self::Opaque { .. }
            | Self::Parameter { .. } => true,
        }
    }
}

impl fmt::Display for CheckedType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inferred => formatter.write_str("_"),
            Self::Error => formatter.write_str("<error>"),
            Self::I32 => formatter.write_str("I32"),
            Self::I8 => formatter.write_str("I8"),
            Self::I16 => formatter.write_str("I16"),
            Self::I64 => formatter.write_str("I64"),
            Self::U8 => formatter.write_str("U8"),
            Self::U16 => formatter.write_str("U16"),
            Self::U32 => formatter.write_str("U32"),
            Self::U64 => formatter.write_str("U64"),
            Self::ISize => formatter.write_str("ISize"),
            Self::USize => formatter.write_str("USize"),
            Self::String => formatter.write_str("String"),
            Self::Ref(value) => format_type_application(formatter, "Ref", value),
            Self::CString => formatter.write_str("CString"),
            Self::CChar => formatter.write_str("CChar"),
            Self::Parameter { name, .. } => formatter.write_str(name),
            Self::TypeConstructor {
                name, arguments, ..
            } => {
                formatter.write_str(name)?;
                for argument in arguments {
                    format_type_argument(formatter, argument)?;
                }
                Ok(())
            }
            Self::Opaque {
                name, arguments, ..
            } => {
                formatter.write_str(name)?;
                for argument in arguments {
                    format_type_argument(formatter, argument)?;
                }
                Ok(())
            }
            Self::CPointer { pointee } => write!(formatter, "CPointer {pointee}"),
            Self::Product(product) => {
                formatter.write_str("(")?;
                for (index, element) in product.elements.iter().enumerate() {
                    if index > 0 {
                        formatter.write_str(", ")?;
                    }
                    if let Some(name) = &element.name {
                        write!(formatter, "{name}: ")?;
                    }
                    write!(formatter, "{}", element.value_type)?;
                }
                if product.variadic {
                    if !product.elements.is_empty() {
                        formatter.write_str(", ")?;
                    }
                    formatter.write_str("...")?;
                }
                formatter.write_str(")")
            }
            Self::Sum(sum) => {
                for (index, alternative) in sum.alternatives.iter().enumerate() {
                    if index > 0 {
                        formatter.write_str(" | ")?;
                    }
                    write!(formatter, "{alternative}")?;
                }
                Ok(())
            }
            Self::Function(function) => {
                write!(formatter, "{} -> {}", function.parameter, function.result)
            }
            Self::Distinct {
                name, arguments, ..
            } => {
                formatter.write_str(name)?;
                for argument in arguments {
                    format_type_argument(formatter, argument)?;
                }
                Ok(())
            }
        }
    }
}

fn format_type_argument(formatter: &mut fmt::Formatter<'_>, argument: &CheckedType) -> fmt::Result {
    if matches!(argument, CheckedType::Sum(_) | CheckedType::Function(_)) {
        write!(formatter, " ({argument})")
    } else {
        write!(formatter, " {argument}")
    }
}

fn format_type_application(
    formatter: &mut fmt::Formatter<'_>,
    name: &str,
    argument: &CheckedType,
) -> fmt::Result {
    formatter.write_str(name)?;
    format_type_argument(formatter, argument)
}

#[derive(Debug, Clone)]
pub struct TypedModule {
    resolved: ResolvedModule,
    expression_types: HashMap<SyntaxId, CheckedType>,
    symbol_types: HashMap<SymbolId, CheckedType>,
    function_types: HashMap<FunctionId, CheckedFunctionType>,
    function_bounds: HashMap<FunctionId, Vec<CheckedTraitBound>>,
    trait_dispatches: HashMap<SyntaxId, CheckedTraitDispatch>,
    trait_implementations: Vec<CheckedTraitImplementation>,
    expression_coercions: HashMap<SyntaxId, CheckedCoercion>,
    propagations: HashMap<SyntaxId, CheckedPropagation>,
    matches: HashMap<SyntaxId, CheckedMatch>,
    accesses: HashMap<SyntaxId, CheckedAccess>,
    pattern_types: HashMap<SyntaxId, CheckedType>,
    ownership: crate::ownership::OwnershipInfo,
    copy_trait: Option<TraitId>,
    drop_trait: Option<TraitId>,
}

impl TypedModule {
    pub fn resolved(&self) -> &ResolvedModule {
        &self.resolved
    }

    pub fn syntax(&self) -> &Module {
        self.resolved.syntax()
    }

    pub fn functions(&self) -> &[ResolvedFunction] {
        self.resolved.functions()
    }

    pub fn symbol_for(&self, syntax_id: SyntaxId) -> Option<SymbolId> {
        self.resolved.symbol_for(syntax_id)
    }

    pub fn function_for(&self, syntax_id: SyntaxId) -> Option<FunctionId> {
        self.resolved.function_for(syntax_id)
    }

    pub fn type_of_expression(&self, syntax_id: SyntaxId) -> Option<&CheckedType> {
        self.expression_types.get(&syntax_id)
    }

    pub fn coercion_for(&self, syntax_id: SyntaxId) -> Option<&CheckedCoercion> {
        self.expression_coercions.get(&syntax_id)
    }

    pub fn propagation_for(&self, syntax_id: SyntaxId) -> Option<&CheckedPropagation> {
        self.propagations.get(&syntax_id)
    }

    pub fn match_for(&self, syntax_id: SyntaxId) -> Option<&CheckedMatch> {
        self.matches.get(&syntax_id)
    }

    pub fn access_for(&self, syntax_id: SyntaxId) -> Option<&CheckedAccess> {
        self.accesses.get(&syntax_id)
    }

    pub fn type_of_pattern(&self, syntax_id: SyntaxId) -> Option<&CheckedType> {
        self.pattern_types.get(&syntax_id)
    }

    pub fn is_copy_type(&self, value_type: &CheckedType) -> bool {
        is_copy_type(
            value_type,
            self.copy_trait,
            self.drop_trait,
            &self.trait_implementations,
            &[],
        )
    }

    pub(crate) fn is_copy_in_function(
        &self,
        value_type: &CheckedType,
        function: Option<FunctionId>,
    ) -> bool {
        // Nested closure functions share their enclosing function's type
        // parameters, but store only their own syntactic bounds. Parameter IDs
        // are globally unique, so including all active function bounds also
        // makes the enclosing `Copy T` obligation visible in the closure.
        let bounds = self
            .function_bounds
            .values()
            .flat_map(|bounds| bounds.iter().cloned())
            .collect::<Vec<_>>();
        let _ = function;
        is_copy_type(
            value_type,
            self.copy_trait,
            self.drop_trait,
            &self.trait_implementations,
            &bounds,
        )
    }

    pub(crate) fn is_drop_method(&self, function: FunctionId) -> bool {
        self.drop_trait.is_some_and(|drop_trait| {
            self.trait_implementations.iter().any(|implementation| {
                implementation.trait_id == drop_trait
                    && implementation
                        .methods
                        .values()
                        .any(|value| *value == function)
            })
        })
    }

    pub fn type_needs_drop(&self, value_type: &CheckedType) -> bool {
        type_needs_drop(value_type, self.drop_trait, &self.trait_implementations)
    }

    pub(crate) fn drop_method_for(&self, value_type: &CheckedType) -> Option<FunctionId> {
        let drop_trait = self.drop_trait?;
        self.trait_implementations
            .iter()
            .find(|implementation| {
                implementation.trait_id == drop_trait && &implementation.target == value_type
            })
            .and_then(|implementation| implementation.methods.values().next().copied())
    }

    pub(crate) fn moved_symbols(&self, syntax: SyntaxId) -> impl Iterator<Item = SymbolId> + '_ {
        self.ownership.moved_symbols(syntax)
    }

    pub(crate) fn is_non_owning_symbol(&self, symbol: SymbolId) -> bool {
        self.ownership.is_non_owning_symbol(symbol)
    }

    pub fn type_of_symbol(&self, symbol: SymbolId) -> Option<&CheckedType> {
        self.symbol_types.get(&symbol)
    }

    pub fn type_of_function(&self, function: FunctionId) -> Option<&CheckedFunctionType> {
        self.function_types.get(&function)
    }

    pub fn bounds_of_function(&self, function: FunctionId) -> &[CheckedTraitBound] {
        self.function_bounds
            .get(&function)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn trait_dispatch_for(&self, syntax: SyntaxId) -> Option<&CheckedTraitDispatch> {
        self.trait_dispatches.get(&syntax)
    }

    pub(crate) fn trait_impl_method(
        &self,
        trait_id: TraitId,
        target: &CheckedType,
        method: TraitMethodId,
    ) -> Option<FunctionId> {
        self.trait_implementations
            .iter()
            .find(|implementation| {
                implementation.trait_id == trait_id && &implementation.target == target
            })
            .and_then(|implementation| implementation.methods.get(&method).copied())
    }
}

#[derive(Default)]
pub struct TypeChecker {
    expression_types: HashMap<SyntaxId, CheckedType>,
    symbol_types: HashMap<SymbolId, CheckedType>,
    function_types: HashMap<FunctionId, CheckedFunctionType>,
    function_bounds: HashMap<FunctionId, Vec<CheckedTraitBound>>,
    trait_method_types: HashMap<TraitMethodId, CheckedType>,
    trait_dispatches: HashMap<SyntaxId, CheckedTraitDispatch>,
    trait_implementations: Vec<CheckedTraitImplementation>,
    impl_function_types: HashMap<FunctionId, CheckedFunctionType>,
    expression_coercions: HashMap<SyntaxId, CheckedCoercion>,
    propagations: HashMap<SyntaxId, CheckedPropagation>,
    matches: HashMap<SyntaxId, CheckedMatch>,
    accesses: HashMap<SyntaxId, CheckedAccess>,
    pattern_types: HashMap<SyntaxId, CheckedType>,
    copy_trait: Option<TraitId>,
    drop_trait: Option<TraitId>,
    active_function_bounds: Vec<Vec<CheckedTraitBound>>,
    function_symbols: HashMap<SymbolId, FunctionId>,
    top_level_bindings: HashMap<SymbolId, Binding>,
    checking_bindings: HashSet<SymbolId>,
    checked_bindings: HashSet<SymbolId>,
    checking_functions: HashSet<FunctionId>,
    checked_functions: HashSet<FunctionId>,
    type_declarations: HashMap<TypeId, TypeDeclaration>,
    resolved_named_types: HashMap<TypeId, CheckedType>,
    resolving_named_types: HashSet<TypeId>,
    return_contexts: Vec<CheckedType>,
    return_contributions: Vec<Vec<ReturnContribution>>,
    pending_propagations: Vec<Vec<(SyntaxId, CheckedType, usize, Span)>>,
    did_return: bool,
    return_reachable: bool,
    diagnostics: Vec<Diagnostic>,
}

impl TypeChecker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn check(mut self, module: ResolvedModule) -> Result<TypedModule, Vec<Diagnostic>> {
        let standard_core = module.program().standard_library_core();
        let standard_trait_syntax = |name: &str| {
            standard_core.and_then(|core| {
                module
                    .program()
                    .module(core)
                    .syntax
                    .items
                    .iter()
                    .find_map(|item| {
                        let Item::TraitDeclaration(declaration) = item else {
                            return None;
                        };
                        (declaration.name == name).then_some(declaration.syntax.id)
                    })
            })
        };
        let copy_syntax = standard_trait_syntax("Copy");
        let drop_syntax = standard_trait_syntax("Drop");
        self.copy_trait = module.traits().iter().find_map(|(id, value)| {
            (Some(value.declaration.syntax.id) == copy_syntax).then_some(*id)
        });
        self.drop_trait = module.traits().iter().find_map(|(id, value)| {
            (Some(value.declaration.syntax.id) == drop_syntax).then_some(*id)
        });
        self.collect_type_declarations(&module);
        self.collect_traits(&module);
        self.collect_trait_implementations(&module);
        self.seed_constructors(&module);
        self.seed_singleton_values(&module);
        self.collect_top_level_bindings(&module);
        self.seed_declared_bindings(&module);
        self.validate_intrinsics(&module);
        self.seed_function_types(&module);

        let module_order = module.program().initialization_order().to_vec();
        for module_id in module_order {
            for item in &module.program().module(module_id).syntax.items {
                self.check_item(&module, item);
            }
        }
        let function_ids = module
            .functions()
            .iter()
            .map(|function| function.id)
            .collect::<Vec<_>>();
        for function_id in function_ids {
            self.ensure_function_checked(&module, function_id);
        }

        if !self.diagnostics.is_empty() {
            return Err(self.diagnostics);
        }

        let typed = TypedModule {
            resolved: module,
            expression_types: self.expression_types,
            symbol_types: self.symbol_types,
            function_types: self.function_types,
            function_bounds: self.function_bounds,
            trait_dispatches: self.trait_dispatches,
            trait_implementations: self.trait_implementations,
            expression_coercions: self.expression_coercions,
            propagations: self.propagations,
            matches: self.matches,
            accesses: self.accesses,
            pattern_types: self.pattern_types,
            ownership: crate::ownership::OwnershipInfo::default(),
            copy_trait: self.copy_trait,
            drop_trait: self.drop_trait,
        };
        let (ownership, ownership_diagnostics) = crate::ownership::OwnershipChecker::check(&typed);
        if ownership_diagnostics.is_empty() {
            Ok(TypedModule { ownership, ..typed })
        } else {
            Err(ownership_diagnostics)
        }
    }

    fn collect_type_declarations(&mut self, module: &ResolvedModule) {
        self.type_declarations = module.type_declarations().clone();
    }

    fn collect_traits(&mut self, module: &ResolvedModule) {
        match self.copy_trait.and_then(|id| module.traits().get(&id)) {
            Some(copy)
                if copy.declaration.visibility == crate::Visibility::Public
                    && copy.declaration.members.is_empty() => {}
            _ => self.diagnostics.push(Diagnostic::new(
                Span::Compiler,
                "standard library must declare public empty trait `Copy`",
            )),
        }
        match self.drop_trait.and_then(|id| module.traits().get(&id)) {
            Some(drop)
                if drop.declaration.visibility == crate::Visibility::Public
                    && drop.declaration.members.len() == 1
                    && drop.declaration.members[0].name == "drop" => {}
            _ => self.diagnostics.push(Diagnostic::new(
                Span::Compiler,
                "standard library must declare public trait `Drop` with one `drop` member",
            )),
        }
        for resolved_trait in module.traits().values() {
            for method in &resolved_trait.methods {
                let member = module.trait_method(*method).expect("resolved trait member");
                let value_type = self.resolve_source_type(module, &member.annotation);
                if !matches!(value_type, CheckedType::Function(_)) {
                    self.diagnostics.push(Diagnostic::new(
                        member.syntax.span.clone(),
                        "trait members must have function types",
                    ));
                }
                if !contains_type_parameter_id(&value_type, resolved_trait.parameter) {
                    self.diagnostics.push(Diagnostic::new(
                        member.syntax.span.clone(),
                        format!(
                            "trait member `{}` must mention trait parameter `{}`",
                            member.name, resolved_trait.declaration.parameter.name
                        ),
                    ));
                }
                if contains_inferred_type(&value_type) {
                    self.diagnostics.push(Diagnostic::new(
                        member.syntax.span.clone(),
                        "trait member types cannot contain `_`",
                    ));
                }
                self.trait_method_types.insert(*method, value_type);
            }
        }
    }

    fn collect_trait_implementations(&mut self, module: &ResolvedModule) {
        for implementation in module.trait_implementations() {
            if Some(implementation.trait_id) == self.copy_trait {
                self.diagnostics.push(Diagnostic::new(
                    implementation.target.syntax().span.clone(),
                    "`Copy` is implemented structurally and cannot be implemented explicitly",
                ));
                continue;
            }
            let target = self.resolve_source_type(module, &implementation.target);
            if contains_type_parameter(&target)
                || contains_inferred_type(&target)
                || !target.is_concrete()
            {
                self.diagnostics.push(Diagnostic::new(
                    implementation.target.syntax().span.clone(),
                    "trait implementation target must be fully concrete",
                ));
                continue;
            }
            if Some(implementation.trait_id) == self.drop_trait
                && !matches!(target, CheckedType::Distinct { .. })
            {
                self.diagnostics.push(Diagnostic::new(
                    implementation.target.syntax().span.clone(),
                    "`Drop` may only be implemented for a represented nominal type",
                ));
                continue;
            }
            if self.trait_implementations.iter().any(|existing| {
                existing.trait_id == implementation.trait_id && existing.target == target
            }) {
                self.diagnostics.push(Diagnostic::new(
                    implementation.target.syntax().span.clone(),
                    format!("duplicate trait implementation for `{target}`"),
                ));
                continue;
            }
            let resolved_trait = &module.traits()[&implementation.trait_id];
            for method in &resolved_trait.methods {
                let member = module.trait_method(*method).expect("resolved trait member");
                let Some(function_id) = implementation.methods.get(method).copied() else {
                    self.diagnostics.push(Diagnostic::new(
                        implementation.target.syntax().span.clone(),
                        format!("implementation is missing member `{}`", member.name),
                    ));
                    continue;
                };
                let mut substitutions = HashMap::new();
                substitutions.insert(resolved_trait.parameter, target.clone());
                let expected =
                    substitute_type(self.trait_method_types[method].clone(), &substitutions);
                if let CheckedType::Function(function_type) = expected {
                    self.impl_function_types.insert(function_id, function_type);
                }
            }
            self.trait_implementations.push(CheckedTraitImplementation {
                trait_id: implementation.trait_id,
                target,
                methods: implementation.methods.clone(),
            });
        }
    }

    fn seed_constructors(&mut self, module: &ResolvedModule) {
        for (symbol, id) in module.constructors() {
            let declaration = self.type_declarations[id].clone();
            let Some(underlying) = declaration.underlying.as_ref() else {
                continue;
            };
            let parameter = self.resolve_source_type(module, underlying);
            let arguments = declaration
                .type_parameters
                .iter()
                .map(|pattern| self.checked_type_parameter_pattern(module, pattern))
                .collect::<Vec<_>>();
            let result = self.instantiate_type_declaration(module, *id, arguments);
            self.symbol_types.insert(
                *symbol,
                CheckedType::Function(CheckedFunctionType {
                    parameter: Box::new(parameter),
                    result: Box::new(result),
                }),
            );
        }
    }

    fn seed_singleton_values(&mut self, module: &ResolvedModule) {
        for (symbol, id) in module.singleton_values() {
            let value_type = self.instantiate_type_declaration(module, *id, Vec::new());
            self.symbol_types.insert(*symbol, value_type);
        }
    }

    fn checked_type_parameter_pattern(
        &self,
        module: &ResolvedModule,
        pattern: &TypeParameterPattern,
    ) -> CheckedType {
        match pattern {
            TypeParameterPattern::Binding(binding) => CheckedType::Parameter {
                id: module
                    .type_parameter_for(binding.syntax.id)
                    .expect("resolved compile-time parameter"),
                name: binding.name.clone(),
            },
            TypeParameterPattern::Product(product) if product.elements.len() == 1 => {
                self.checked_type_parameter_pattern(module, &product.elements[0])
            }
            TypeParameterPattern::Product(product) => CheckedType::Product(CheckedProductType {
                elements: product
                    .elements
                    .iter()
                    .map(|element| CheckedTypeElement {
                        name: None,
                        value_type: self.checked_type_parameter_pattern(module, element),
                    })
                    .collect(),
                variadic: false,
            }),
        }
    }

    fn collect_top_level_bindings(&mut self, module: &ResolvedModule) {
        for source_module in module.program().modules() {
            for item in &source_module.syntax.items {
                if let Item::Statement(statement) = item
                    && let Statement::Binding(binding) = statement.as_ref()
                    && let Some(symbol) = module.symbol_for(binding.syntax.id)
                {
                    self.top_level_bindings.insert(symbol, binding.clone());
                }
            }
        }
    }

    fn seed_declared_bindings(&mut self, module: &ResolvedModule) {
        for source_module in module.program().modules() {
            for item in &source_module.syntax.items {
                match item {
                    Item::ExternBlock(block) => {
                        for binding in &block.bindings {
                            self.seed_binding_annotation(module, binding);
                        }
                    }
                    Item::Statement(statement) => {
                        if let Statement::Binding(binding) = statement.as_ref() {
                            self.seed_binding_annotation(module, binding);
                        }
                    }
                    Item::UseDeclaration(_)
                    | Item::TypeDeclaration(_)
                    | Item::MacroDeclaration(_)
                    | Item::TraitDeclaration(_)
                    | Item::TraitImplementation(_) => {}
                }
            }
        }
    }

    fn seed_binding_annotation(&mut self, module: &ResolvedModule, binding: &Binding) {
        let Some(annotation) = &binding.annotation else {
            return;
        };
        let value_type = self.resolve_source_type(module, annotation);
        if let Some(symbol) = module.symbol_for(binding.syntax.id) {
            self.symbol_types.insert(symbol, value_type);
        }
    }

    fn validate_intrinsics(&mut self, module: &ResolvedModule) {
        for (symbol, intrinsic) in module.intrinsic_functions() {
            let expected = match intrinsic {
                crate::IntrinsicFunction::IntegerBinary { integer, .. } => {
                    let integer = CheckedType::integer(*integer);
                    CheckedType::Function(CheckedFunctionType {
                        parameter: Box::new(CheckedType::Product(CheckedProductType {
                            elements: vec![
                                CheckedTypeElement {
                                    name: None,
                                    value_type: integer.clone(),
                                },
                                CheckedTypeElement {
                                    name: None,
                                    value_type: integer.clone(),
                                },
                            ],
                            variadic: false,
                        })),
                        result: Box::new(integer),
                    })
                }
                crate::IntrinsicFunction::IntegerCompare { integer, .. } => {
                    let integer = CheckedType::integer(*integer);
                    let result = self.symbol_types.get(symbol).and_then(|value| match value {
                        CheckedType::Function(function) => Some(function.result.as_ref().clone()),
                        _ => None,
                    }).filter(|value| matches!(value,
                        CheckedType::Sum(sum) if sum.alternatives.len() == 2
                            && matches!(&sum.alternatives[0], CheckedType::Distinct { name, .. } if name.ends_with("True"))
                            && matches!(&sum.alternatives[1], CheckedType::Distinct { name, .. } if name.ends_with("False"))
                    )).unwrap_or(CheckedType::Error);
                    CheckedType::Function(CheckedFunctionType {
                        parameter: Box::new(CheckedType::Product(CheckedProductType {
                            elements: vec![
                                CheckedTypeElement {
                                    name: None,
                                    value_type: integer.clone(),
                                },
                                CheckedTypeElement {
                                    name: None,
                                    value_type: integer,
                                },
                            ],
                            variadic: false,
                        })),
                        result: Box::new(result),
                    })
                }
                crate::IntrinsicFunction::StringFromCString => {
                    CheckedType::Function(CheckedFunctionType {
                        parameter: Box::new(CheckedType::CString),
                        result: Box::new(CheckedType::String),
                    })
                }
                crate::IntrinsicFunction::StringToCString => {
                    CheckedType::Function(CheckedFunctionType {
                        parameter: Box::new(CheckedType::String),
                        result: Box::new(CheckedType::CString),
                    })
                }
                crate::IntrinsicFunction::Drop => self
                    .symbol_types
                    .get(symbol)
                    .cloned()
                    .filter(|value_type| {
                        matches!(value_type,
                            CheckedType::Function(function)
                                if contains_type_parameter(&function.parameter)
                                    && *function.result == CheckedType::empty_product())
                    })
                    .unwrap_or(CheckedType::Error),
            };
            if self.symbol_types.get(symbol) != Some(&expected) {
                self.diagnostics.push(Diagnostic::new(
                    Span::Compiler,
                    "standard-library intrinsic has an invalid type",
                ));
            }
        }
    }

    fn seed_function_types(&mut self, module: &ResolvedModule) {
        for function in module.functions() {
            if let Some(expected) = self.impl_function_types.get(&function.id).cloned() {
                self.function_types.insert(function.id, expected);
                continue;
            }
            let mut parameter = self.resolve_source_type(module, &function.pattern.ty());
            if !parameter.is_concrete()
                && let Some(annotation) = &function.binding_annotation
                && let CheckedType::Function(annotation) =
                    self.resolve_source_type(module, annotation)
            {
                parameter = *annotation.parameter;
            }
            let result = function
                .result_annotation
                .as_ref()
                .map(|annotation| self.resolve_source_type(module, annotation))
                .or_else(|| {
                    function.binding_annotation.as_ref().and_then(|annotation| {
                        let CheckedType::Function(function_type) =
                            self.resolve_source_type(module, annotation)
                        else {
                            return None;
                        };
                        Some(*function_type.result)
                    })
                })
                .unwrap_or(CheckedType::Inferred);
            let mut function_type = CheckedFunctionType {
                parameter: Box::new(parameter),
                result: Box::new(result),
            };

            if let Some(annotation) = &function.binding_annotation {
                let binding_type = self.resolve_source_type(module, annotation);
                let actual = CheckedType::Function(function_type.clone());
                let merged =
                    self.require_compatible(actual, binding_type, annotation.syntax().span.clone());
                if let CheckedType::Function(merged_function) = merged {
                    function_type = merged_function;
                }
            }

            if let Some(binding_syntax) = function.binding_syntax
                && let Some(symbol) = module.symbol_for(binding_syntax)
            {
                self.function_symbols.insert(symbol, function.id);
                self.symbol_types
                    .insert(symbol, CheckedType::Function(function_type.clone()));
            }
            self.function_types.insert(function.id, function_type);
            let bounds = function
                .trait_bounds
                .iter()
                .filter_map(|bound| {
                    Some(CheckedTraitBound {
                        trait_id: module.trait_for(bound.syntax.id)?,
                        argument: self.resolve_source_type(module, &bound.argument),
                    })
                })
                .collect::<Vec<_>>();
            if !bounds.is_empty() {
                self.function_bounds.insert(function.id, bounds);
            }
        }
    }

    fn ensure_function_checked(&mut self, module: &ResolvedModule, function_id: FunctionId) {
        if self.checked_functions.contains(&function_id)
            || !self.checking_functions.insert(function_id)
        {
            return;
        }
        let function = module
            .functions()
            .iter()
            .find(|function| function.id == function_id)
            .cloned()
            .expect("resolved function ID must be valid");
        let function_type = self.function_types[&function.id].clone();
        self.active_function_bounds.push(
            self.function_bounds
                .get(&function.id)
                .cloned()
                .unwrap_or_default(),
        );
        self.bind_pattern_types(module, &function.pattern, &function_type.parameter);
        let outer_did_return = self.did_return;
        let outer_return_reachable = self.return_reachable;
        self.did_return = false;
        self.return_reachable = true;
        self.return_contexts.push((*function_type.result).clone());
        self.return_contributions.push(Vec::new());
        self.pending_propagations.push(Vec::new());
        let body_expected = (!matches!(*function_type.result, CheckedType::Inferred))
            .then_some(function_type.result.as_ref());
        let body_type = self.check_expression_expected(module, &function.body, body_expected);
        let returned = self.did_return;
        let declared_result = self.return_contexts.pop().expect("function return context");
        let mut contributions = self
            .return_contributions
            .pop()
            .expect("function return contributions");
        if !returned {
            contributions.push(ReturnContribution {
                syntax: Some(function.body.syntax().id),
                span: function.body.syntax().span.clone(),
                value_type: body_type,
            });
        }
        let result_type = if declared_result == CheckedType::Inferred {
            self.join_return_contributions(&contributions, function.body.syntax().span.clone())
        } else {
            declared_result
        };
        for contribution in contributions {
            if let Some(syntax) = contribution.syntax {
                self.coerce_expression_type(
                    syntax,
                    contribution.value_type,
                    &result_type,
                    contribution.span,
                );
            } else if !can_coerce_type(&contribution.value_type, &result_type) {
                self.diagnostics.push(Diagnostic::new(
                    contribution.span,
                    format!(
                        "propagated variant `{}` is not contained in function result `{result_type}`",
                        contribution.value_type
                    ),
                ));
            }
        }
        let pending = self
            .pending_propagations
            .pop()
            .expect("function propagations");
        for (syntax, source, success_index, _) in pending {
            self.propagations.insert(
                syntax,
                CheckedPropagation {
                    source,
                    success_index,
                    result: result_type.clone(),
                },
            );
        }
        self.did_return = outer_did_return;
        self.return_reachable = outer_return_reachable;
        let checked_function_type = CheckedFunctionType {
            parameter: function_type.parameter,
            result: Box::new(result_type),
        };
        self.function_types
            .insert(function.id, checked_function_type.clone());
        if let Some(binding_syntax) = function.binding_syntax
            && let Some(symbol) = module.symbol_for(binding_syntax)
        {
            self.symbol_types
                .insert(symbol, CheckedType::Function(checked_function_type));
        }
        self.checking_functions.remove(&function_id);
        self.checked_functions.insert(function_id);
        self.active_function_bounds.pop();
    }

    fn join_return_contributions(
        &mut self,
        contributions: &[ReturnContribution],
        span: Span,
    ) -> CheckedType {
        let mut values = contributions
            .iter()
            .map(|contribution| contribution.value_type.clone())
            .filter(|value_type| *value_type != CheckedType::Error)
            .collect::<Vec<_>>();
        if values.is_empty() {
            return CheckedType::empty_product();
        }
        if values.iter().all(|value_type| value_type == &values[0]) {
            return values.remove(0);
        }
        self.normalize_sum_type(values, span)
    }

    fn bind_pattern_types(
        &mut self,
        module: &ResolvedModule,
        pattern: &Pattern,
        value_type: &CheckedType,
    ) {
        self.pattern_types
            .insert(pattern.syntax().id, value_type.clone());
        match pattern {
            Pattern::Wildcard(_) => {}
            Pattern::Binding(binding) => {
                let value_type = if matches!(binding.ty, Type::Inferred(_)) {
                    value_type.clone()
                } else {
                    let declared = self.resolve_source_type(module, &binding.ty);
                    self.require_compatible(
                        value_type.clone(),
                        declared,
                        binding.syntax.span.clone(),
                    )
                };
                if let Some(symbol) = module.symbol_for(binding.syntax.id) {
                    self.symbol_types.insert(symbol, value_type);
                }
            }
            Pattern::Product(product) if product.elements.len() == 1 => {
                self.bind_pattern_types(module, &product.elements[0], value_type);
            }
            Pattern::Product(product) => {
                let CheckedType::Product(product_type) = value_type else {
                    if *value_type != CheckedType::Error {
                        self.diagnostics.push(Diagnostic::new(
                            product.syntax.span.clone(),
                            format!("product pattern cannot match `{value_type}`"),
                        ));
                    }
                    return;
                };
                if product.elements.len() != product_type.elements.len() {
                    self.diagnostics.push(Diagnostic::new(
                        product.syntax.span.clone(),
                        format!(
                            "product pattern has {} elements but value has {}",
                            product.elements.len(),
                            product_type.elements.len()
                        ),
                    ));
                    return;
                }
                for (pattern, element) in product.elements.iter().zip(&product_type.elements) {
                    self.bind_pattern_types(module, pattern, &element.value_type);
                }
            }
            Pattern::Nominal(pattern) => {
                let Some(expected_id) = module.type_for_pattern(pattern.syntax.id) else {
                    return;
                };
                match value_type {
                    CheckedType::Ref(payload)
                        if module.builtin_type(expected_id) == Some(BuiltinType::Ref) =>
                    {
                        self.bind_pattern_types(module, &pattern.argument, payload);
                    }
                    CheckedType::Distinct {
                        id, representation, ..
                    } if *id == expected_id => {
                        self.bind_pattern_types(module, &pattern.argument, representation);
                    }
                    CheckedType::Error => {}
                    other => self.diagnostics.push(Diagnostic::new(
                        pattern.syntax.span.clone(),
                        format!("nominal pattern `{}` cannot match `{other}`", pattern.name),
                    )),
                }
            }
        }
    }

    fn check_item(&mut self, module: &ResolvedModule, item: &Item) {
        match item {
            Item::ExternBlock(block) => {
                for binding in &block.bindings {
                    if !binding.type_parameters.is_empty() {
                        self.diagnostics.push(Diagnostic::new(
                            binding.syntax.span.clone(),
                            "external bindings cannot have compile-time parameters",
                        ));
                    }
                    if block.abi != "\"staple-intrinsic\"" {
                        let external_type = binding
                            .annotation
                            .as_ref()
                            .map(|annotation| self.resolve_source_type(module, annotation));
                        if external_type
                            .as_ref()
                            .is_some_and(checked_type_contains_sum)
                        {
                            self.diagnostics.push(Diagnostic::new(
                                binding.syntax.span.clone(),
                                "external binding types cannot contain sums",
                            ));
                        }
                        if matches!(
                            external_type,
                            Some(CheckedType::Function(CheckedFunctionType { result, .. }))
                                if checked_type_contains_cstring(&result)
                        ) {
                            self.diagnostics.push(Diagnostic::new(
                                binding.syntax.span.clone(),
                                "external functions cannot return owned `CString` values",
                            ));
                        }
                    }
                    self.check_binding(module, binding);
                }
            }
            Item::Statement(statement) => {
                self.check_statement(module, statement);
            }
            Item::UseDeclaration(_)
            | Item::TypeDeclaration(_)
            | Item::MacroDeclaration(_)
            | Item::TraitDeclaration(_)
            | Item::TraitImplementation(_) => {}
        }
    }

    fn check_statement(&mut self, module: &ResolvedModule, statement: &Statement) -> CheckedType {
        match statement {
            Statement::Binding(binding) => {
                self.check_binding(module, binding);
                CheckedType::empty_product()
            }
            Statement::PatternBinding(binding) => {
                let value_type = self.check_expression(module, &binding.value);
                if !self.did_return {
                    if binding.kind == PatternBindingKind::Propagating {
                        self.check_propagating_binding(module, binding, &value_type);
                    } else {
                        self.bind_pattern_types(module, &binding.pattern, &value_type);
                    }
                }
                CheckedType::empty_product()
            }
            Statement::Return(statement) => {
                let expected = self.return_contexts.last().cloned();
                let concrete_expected = expected
                    .as_ref()
                    .filter(|value_type| **value_type != CheckedType::Inferred);
                let value_type =
                    self.check_expression_expected(module, &statement.value, concrete_expected);
                if !self.did_return && self.return_reachable {
                    self.return_contributions
                        .last_mut()
                        .expect("return contribution inside function")
                        .push(ReturnContribution {
                            syntax: Some(statement.value.syntax().id),
                            span: statement.value.syntax().span.clone(),
                            value_type,
                        });
                    self.did_return = true;
                }
                CheckedType::empty_product()
            }
            Statement::Expression(expression) => self.check_expression(module, expression),
        }
    }

    fn check_propagating_binding(
        &mut self,
        module: &ResolvedModule,
        binding: &crate::PatternBinding,
        value_type: &CheckedType,
    ) {
        let Pattern::Nominal(pattern) = &binding.pattern else {
            return;
        };
        let CheckedType::Sum(sum) = value_type else {
            if *value_type != CheckedType::Error {
                self.diagnostics.push(Diagnostic::new(
                    binding.value.syntax().span.clone(),
                    format!("a propagating binding requires a sum value, found `{value_type}`"),
                ));
            }
            return;
        };
        let Some(expected_id) = module.type_for_pattern(pattern.syntax.id) else {
            return;
        };
        let matches = sum
            .alternatives
            .iter()
            .enumerate()
            .filter(|(_, alternative)| {
                matches!(alternative, CheckedType::Distinct { id, .. } if *id == expected_id)
            })
            .collect::<Vec<_>>();
        let [(success_index, CheckedType::Distinct { representation, .. })] = matches.as_slice()
        else {
            self.diagnostics.push(Diagnostic::new(
                pattern.syntax.span.clone(),
                format!("nominal pattern `{}` does not select exactly one alternative of `{value_type}`", pattern.name),
            ));
            return;
        };
        self.bind_pattern_types(module, &pattern.argument, representation);
        let contributions = self
            .return_contributions
            .last_mut()
            .expect("propagation inside function");
        for (index, alternative) in sum.alternatives.iter().enumerate() {
            if index != *success_index {
                contributions.push(ReturnContribution {
                    syntax: None,
                    span: binding.syntax.span.clone(),
                    value_type: alternative.clone(),
                });
            }
        }
        self.pending_propagations
            .last_mut()
            .expect("propagation inside function")
            .push((
                binding.syntax.id,
                value_type.clone(),
                *success_index,
                binding.syntax.span.clone(),
            ));
    }

    fn check_binding(&mut self, module: &ResolvedModule, binding: &Binding) {
        let symbol = module.symbol_for(binding.syntax.id);
        if let Some(symbol) = symbol {
            if self.checked_bindings.contains(&symbol) {
                return;
            }
            if self.top_level_bindings.contains_key(&symbol)
                && !self.checking_bindings.insert(symbol)
            {
                return;
            }
        }
        let declared_type = binding
            .annotation
            .as_ref()
            .map(|annotation| self.resolve_source_type(module, annotation));
        let generic_annotation_is_function =
            matches!(declared_type, Some(CheckedType::Function(_)));
        let value_type = binding
            .value
            .as_ref()
            .map(|value| self.check_expression_expected(module, value, declared_type.as_ref()));

        let checked_type = if self.did_return {
            declared_type.clone().unwrap_or(CheckedType::Error)
        } else {
            match (value_type, declared_type) {
                (Some(actual), Some(expected)) => {
                    self.require_compatible(actual, expected, binding.syntax.span.clone())
                }
                (Some(actual), None) => actual,
                (None, Some(expected)) => expected,
                (None, None) => {
                    self.diagnostics.push(Diagnostic::new(
                        binding.syntax.span.clone(),
                        format!(
                            "cannot infer the type of `{}` without a value",
                            binding.name
                        ),
                    ));
                    CheckedType::Error
                }
            }
        };
        if !checked_type.is_concrete() && checked_type != CheckedType::Error {
            self.diagnostics.push(Diagnostic::new(
                binding.syntax.span.clone(),
                format!("could not fully infer the type of `{}`", binding.name),
            ));
        }
        if binding.type_parameters.is_empty()
            && contains_type_parameter(&checked_type)
            && checked_type != CheckedType::Error
        {
            self.diagnostics.push(Diagnostic::new(
                binding.syntax.span.clone(),
                format!(
                    "generic value `{}` requires a concrete expected type",
                    binding.name
                ),
            ));
        }
        if !binding.type_parameters.is_empty() && !generic_annotation_is_function {
            self.diagnostics.push(Diagnostic::new(
                binding.syntax.span.clone(),
                "compile-time parameters require a function-valued `def`",
            ));
        }
        if !binding.type_parameters.is_empty()
            && !matches!(binding.value, Some(Expression::Function(_)))
        {
            self.diagnostics.push(Diagnostic::new(
                binding.syntax.span.clone(),
                "a generic `def` requires a function body",
            ));
        }
        if let Some(symbol) = symbol {
            self.symbol_types.insert(symbol, checked_type);
            if self.top_level_bindings.contains_key(&symbol) {
                self.checking_bindings.remove(&symbol);
                self.checked_bindings.insert(symbol);
            }
        }
    }

    fn ensure_binding_checked(&mut self, module: &ResolvedModule, symbol: SymbolId) {
        if self.symbol_types.contains_key(&symbol) || self.checking_bindings.contains(&symbol) {
            return;
        }
        if let Some(binding) = self.top_level_bindings.get(&symbol).cloned() {
            self.check_binding(module, &binding);
        }
    }

    fn check_expression(
        &mut self,
        module: &ResolvedModule,
        expression: &Expression,
    ) -> CheckedType {
        self.check_expression_expected(module, expression, None)
    }

    fn check_expression_expected(
        &mut self,
        module: &ResolvedModule,
        expression: &Expression,
        expected: Option<&CheckedType>,
    ) -> CheckedType {
        let trait_methods = module.trait_methods_for_expression(expression.syntax().id);
        if !trait_methods.is_empty() && !matches!(expression, Expression::Call(_)) {
            let value_type = self.resolve_trait_method_use(
                module,
                expression.syntax().id,
                trait_methods,
                None,
                expected,
                expression.syntax().span.clone(),
            );
            return self.finish_expression_type(expression, value_type, expected);
        }
        let natural_type = match expression {
            Expression::Function(function) => {
                let Some(function_id) = module.function_for(function.syntax.id) else {
                    return CheckedType::Error;
                };
                if !self.checked_functions.contains(&function_id)
                    && let Some(CheckedType::Function(expected)) = expected
                    && let Some(current) = self.function_types.get(&function_id).cloned()
                    && let Some(CheckedType::Function(merged)) = merge_types(
                        CheckedType::Function(current),
                        CheckedType::Function(expected.clone()),
                    )
                {
                    self.function_types.insert(function_id, merged);
                }
                self.ensure_function_checked(module, function_id);
                self.function_types
                    .get(&function_id)
                    .cloned()
                    .map(CheckedType::Function)
                    .unwrap_or(CheckedType::Error)
            }
            Expression::Satisfies(satisfies) => {
                let annotation = self.resolve_source_type(module, &satisfies.ty);
                self.check_expression_expected(module, &satisfies.value, Some(&annotation))
            }
            Expression::Match(match_) => self.check_match_expression(module, match_, expected),
            Expression::Block(block) => {
                let mut result = CheckedType::empty_product();
                let mut block_returned = false;
                for (index, statement) in block.statements.iter().enumerate() {
                    let outer_reachable = self.return_reachable;
                    if block_returned {
                        self.return_reachable = false;
                        self.did_return = false;
                    }
                    if !block_returned
                        && index + 1 == block.statements.len()
                        && let Statement::Expression(expression) = statement
                    {
                        result = self.check_expression_expected(module, expression, expected);
                    } else {
                        result = self.check_statement(module, statement);
                    }
                    if block_returned {
                        self.did_return = true;
                    } else if self.did_return {
                        block_returned = true;
                    }
                    self.return_reachable = outer_reachable;
                }
                result
            }
            Expression::Product(product) => {
                let mut elements = Vec::new();
                for (index, element) in product.elements.iter().enumerate() {
                    let value_type = self.check_expression_expected(
                        module,
                        &element.value,
                        match expected {
                            Some(CheckedType::Product(product)) => product
                                .elements
                                .get(index)
                                .map(|element| &element.value_type),
                            other if product.elements.len() == 1 => other,
                            _ => None,
                        },
                    );
                    if self.did_return {
                        return CheckedType::empty_product();
                    }
                    elements.push(CheckedTypeElement {
                        name: element.name.clone(),
                        value_type,
                    });
                }
                normalize_product_type(elements, false)
            }
            Expression::Call(call) => {
                if module.primitive_macro_for(call.syntax.id).is_some() {
                    self.check_expression(module, &call.argument);
                    if self.did_return {
                        return CheckedType::empty_product();
                    }
                    if !matches!(call.argument.as_ref(), Expression::String(_)) {
                        self.diagnostics.push(Diagnostic::new(
                            call.argument.syntax().span.clone(),
                            "`c_string` requires a string literal",
                        ));
                        return CheckedType::Error;
                    }
                    CheckedType::CString
                } else {
                    let trait_methods =
                        module.trait_methods_for_expression(call.callee.syntax().id);
                    if !trait_methods.is_empty() {
                        let argument_type = self.check_expression(module, &call.argument);
                        if self.did_return {
                            return CheckedType::empty_product();
                        }
                        let callee_type = self.resolve_trait_method_use(
                            module,
                            call.callee.syntax().id,
                            trait_methods,
                            Some(&argument_type),
                            expected,
                            call.callee.syntax().span.clone(),
                        );
                        self.expression_types
                            .insert(call.callee.syntax().id, callee_type.clone());
                        let result = match callee_type {
                            CheckedType::Function(function) => {
                                self.check_call_argument(
                                    argument_type,
                                    &function.parameter,
                                    call.argument.syntax().span.clone(),
                                );
                                *function.result
                            }
                            other => other,
                        };
                        return self.finish_expression_type(expression, result, expected);
                    }
                    let mut raw_callee_type = self.check_expression(module, &call.callee);
                    if self.did_return {
                        return CheckedType::empty_product();
                    }
                    let argument_expected = match &raw_callee_type {
                        CheckedType::Function(function)
                            if !contains_type_parameter(&raw_callee_type) =>
                        {
                            Some(function.parameter.as_ref())
                        }
                        _ => None,
                    };
                    let argument_type =
                        self.check_expression_expected(module, &call.argument, argument_expected);
                    if self.did_return {
                        return CheckedType::empty_product();
                    }
                    if let Some(expected_result) = expected
                        && !matches!(expected_result, CheckedType::Sum(_))
                    {
                        let expected_callee = CheckedType::Function(CheckedFunctionType {
                            parameter: Box::new(argument_type.clone()),
                            result: Box::new(expected_result.clone()),
                        });
                        raw_callee_type = self.check_expression_expected(
                            module,
                            &call.callee,
                            Some(&expected_callee),
                        );
                    }
                    let callee_type = self.instantiate_function_use(
                        raw_callee_type.clone(),
                        Some(&argument_type),
                        expected,
                        call.callee.syntax().span.clone(),
                    );
                    if let Some(function_id) = self.function_origin(module, &call.callee) {
                        let bound_template = self
                            .function_types
                            .get(&function_id)
                            .cloned()
                            .map(CheckedType::Function)
                            .unwrap_or_else(|| raw_callee_type.clone());
                        self.check_function_bounds(
                            function_id,
                            &bound_template,
                            &callee_type,
                            call.callee.syntax().span.clone(),
                        );
                    }
                    self.expression_types
                        .insert(call.callee.syntax().id, callee_type.clone());
                    match callee_type {
                        CheckedType::Function(function) => {
                            self.check_call_argument(
                                argument_type,
                                &function.parameter,
                                call.argument.syntax().span.clone(),
                            );
                            *function.result
                        }
                        CheckedType::Error => CheckedType::Error,
                        other => {
                            self.diagnostics.push(Diagnostic::new(
                                call.callee.syntax().span.clone(),
                                format!("cannot call a value of type `{other}`"),
                            ));
                            CheckedType::Error
                        }
                    }
                }
            }
            Expression::Access(access) => {
                if let Some(symbol) = module.symbol_for(access.syntax.id) {
                    self.ensure_binding_checked(module, symbol);
                    if let Some(function_id) = self.function_symbols.get(&symbol).copied() {
                        self.ensure_function_checked(module, function_id);
                    }
                    let raw_type = self.symbol_types.get(&symbol).cloned().unwrap_or_else(|| {
                        self.diagnostics.push(Diagnostic::new(
                            access.syntax.span.clone(),
                            "the type of the imported value is not available here",
                        ));
                        CheckedType::Error
                    });
                    let value_type = self.instantiate_function_use(
                        raw_type,
                        None,
                        expected,
                        access.syntax.span.clone(),
                    );
                    return self.finish_expression_type(expression, value_type, expected);
                }
                let value_type = self.check_expression(module, &access.value);
                if self.did_return {
                    return CheckedType::empty_product();
                }
                let mut accessible = value_type.clone();
                let mut dereference = None;
                accessible = loop {
                    match accessible {
                        CheckedType::Distinct { representation, .. } => {
                            accessible = *representation;
                        }
                        CheckedType::Ref(payload) if dereference.is_none() => {
                            dereference = Some(payload.as_ref().clone());
                            accessible = *payload;
                        }
                        other => break other,
                    }
                };
                match accessible {
                    CheckedType::Product(product) => {
                        let index = match &access.accessor {
                            Accessor::Index(index) => index.parse::<usize>().ok(),
                            Accessor::Name(name) => product
                                .elements
                                .iter()
                                .position(|element| element.name.as_deref() == Some(name)),
                        };
                        let Some(index) = index else {
                            self.diagnostics.push(Diagnostic::new(
                                access.syntax.span.clone(),
                                match &access.accessor {
                                    Accessor::Index(index) => {
                                        format!("product index `{index}` is out of bounds")
                                    }
                                    Accessor::Name(name) => {
                                        format!("product has no element named `{name}`")
                                    }
                                },
                            ));
                            return CheckedType::Error;
                        };
                        let Some(element) = product.elements.get(index) else {
                            self.diagnostics.push(Diagnostic::new(
                                access.syntax.span.clone(),
                                format!("product index `{index}` is out of bounds"),
                            ));
                            return CheckedType::Error;
                        };
                        self.accesses
                            .insert(access.syntax.id, CheckedAccess { index, dereference });
                        element.value_type.clone()
                    }
                    CheckedType::Error => CheckedType::Error,
                    other => {
                        self.diagnostics.push(Diagnostic::new(
                            access.value.syntax().span.clone(),
                            format!("cannot access an element of `{other}`"),
                        ));
                        CheckedType::Error
                    }
                }
            }
            Expression::Infix(infix) => module
                .lowered_infix(infix.syntax.id)
                .cloned()
                .map(|lowered| self.check_expression_expected(module, &lowered, expected))
                .unwrap_or(CheckedType::Error),
            Expression::Name(name) => {
                let symbol = module.symbol_for(name.syntax.id);
                if let Some(symbol) = symbol {
                    self.ensure_binding_checked(module, symbol);
                }
                if let Some(function_id) =
                    symbol.and_then(|symbol| self.function_symbols.get(&symbol).copied())
                {
                    self.ensure_function_checked(module, function_id);
                }
                let raw = symbol
                    .and_then(|symbol| self.symbol_types.get(&symbol).cloned())
                    .unwrap_or_else(|| {
                        self.diagnostics.push(Diagnostic::new(
                            name.syntax.span.clone(),
                            format!("the type of `{}` is not available here", name.name),
                        ));
                        CheckedType::Error
                    });
                let instantiated = self.instantiate_function_use(
                    raw.clone(),
                    None,
                    expected,
                    name.syntax.span.clone(),
                );
                if expected.is_some()
                    && let Some(function_id) =
                        symbol.and_then(|symbol| self.function_symbols.get(&symbol).copied())
                {
                    self.check_function_bounds(
                        function_id,
                        &raw,
                        &instantiated,
                        name.syntax.span.clone(),
                    );
                }
                instantiated
            }
            Expression::Quote(_) | Expression::Splice(_) => CheckedType::Error,
            Expression::String(_) => CheckedType::String,
            Expression::CString(_) => CheckedType::CString,
            Expression::Integer(integer) => {
                let integer_type = expected
                    .and_then(CheckedType::integer_type)
                    .unwrap_or(IntegerType::I32);
                if let Some(width) = integer_type.fixed_width()
                    && integer.literal.parse::<u128>().ok().is_none_or(|value| {
                        let value_bits = if integer_type.is_signed() {
                            width - 1
                        } else {
                            width
                        };
                        value > ((1_u128 << value_bits) - 1)
                    })
                {
                    self.diagnostics.push(Diagnostic::new(
                        integer.syntax.span.clone(),
                        format!(
                            "integer literal `{}` does not fit in `{}`",
                            integer.literal,
                            integer_type.name()
                        ),
                    ));
                }
                CheckedType::integer(integer_type)
            }
        };
        self.finish_expression_type(expression, natural_type, expected)
    }

    fn check_match_expression(
        &mut self,
        module: &ResolvedModule,
        match_: &crate::MatchExpression,
        expected: Option<&CheckedType>,
    ) -> CheckedType {
        let source = self.check_expression(module, &match_.subject);
        if self.did_return {
            return CheckedType::empty_product();
        }
        if !matches!(
            source,
            CheckedType::Sum(_) | CheckedType::Product(_) | CheckedType::Ref(_)
        ) {
            if source != CheckedType::Error {
                self.diagnostics.push(Diagnostic::new(
                    match_.subject.syntax().span.clone(),
                    format!("a match subject must have a sum or product type, found `{source}`"),
                ));
            }
            return CheckedType::Error;
        }

        let outer_reachable = self.return_reachable;
        let mut previous_patterns = Vec::new();
        let mut values = Vec::new();
        let mut every_arm_returns = true;

        for arm in &match_.arms {
            self.check_match_pattern(module, &arm.pattern, &source);
            if !Self::match_pattern_is_useful(module, &source, &previous_patterns, &arm.pattern) {
                self.diagnostics.push(Diagnostic::new(
                    arm.pattern.syntax().span.clone(),
                    "unreachable match arm",
                ));
            }
            previous_patterns.push(&arm.pattern);

            self.did_return = false;
            self.return_reachable = outer_reachable;
            let value_type = self.check_expression_expected(module, &arm.body, expected);
            if self.did_return {
                // This arm contributes no value to the match result.
            } else {
                every_arm_returns = false;
                values.push(ReturnContribution {
                    syntax: Some(arm.body.syntax().id),
                    span: arm.body.syntax().span.clone(),
                    value_type,
                });
            }
        }

        if Self::match_pattern_is_useful(
            module,
            &source,
            &previous_patterns,
            &Pattern::Wildcard(crate::WildcardPattern {
                syntax: match_.syntax.clone(),
            }),
        ) {
            let missing = if let CheckedType::Sum(sum) = &source {
                let matrix = previous_patterns
                    .iter()
                    .map(|pattern| vec![CoveragePattern::Pattern(pattern)])
                    .collect::<Vec<_>>();
                let alternatives = sum
                    .alternatives
                    .iter()
                    .enumerate()
                    .filter_map(|(index, alternative)| {
                        let CheckedType::Distinct { representation, .. } = alternative else {
                            return None;
                        };
                        let specialized = matrix
                            .iter()
                            .filter_map(|row| Self::specialize_sum_row(module, row, index, sum))
                            .collect::<Vec<_>>();
                        Self::coverage_is_useful(
                            module,
                            &[representation.as_ref().clone()],
                            &specialized,
                            &[CoveragePattern::Any],
                        )
                        .then(|| format!("`{alternative}`"))
                    })
                    .collect::<Vec<_>>();
                (!alternatives.is_empty()).then(|| alternatives.join(", "))
            } else {
                None
            };
            self.diagnostics.push(Diagnostic::new(
                match_.syntax.span.clone(),
                missing.map_or_else(
                    || "non-exhaustive match".to_owned(),
                    |missing| format!("non-exhaustive match; missing {missing}"),
                ),
            ));
        }

        let result = if every_arm_returns {
            CheckedType::empty_product()
        } else if let Some(expected) = expected {
            expected.clone()
        } else {
            self.join_return_contributions(&values, match_.syntax.span.clone())
        };
        if expected.is_none() {
            for value in &values {
                self.coerce_expression_type(
                    value.syntax.expect("match arm expression"),
                    value.value_type.clone(),
                    &result,
                    value.span.clone(),
                );
            }
        }
        self.matches
            .insert(match_.syntax.id, CheckedMatch { source });
        self.did_return = every_arm_returns;
        self.return_reachable = outer_reachable;
        result
    }

    fn check_match_pattern(
        &mut self,
        module: &ResolvedModule,
        pattern: &Pattern,
        value_type: &CheckedType,
    ) {
        self.pattern_types
            .insert(pattern.syntax().id, value_type.clone());
        match pattern {
            Pattern::Binding(_) | Pattern::Wildcard(_) => {
                self.bind_pattern_types(module, pattern, value_type);
            }
            Pattern::Product(product) if product.elements.len() == 1 => {
                self.check_match_pattern(module, &product.elements[0], value_type);
            }
            Pattern::Product(product) => {
                let CheckedType::Product(value) = value_type else {
                    if *value_type != CheckedType::Error {
                        self.diagnostics.push(Diagnostic::new(
                            product.syntax.span.clone(),
                            format!("product pattern cannot match `{value_type}`"),
                        ));
                    }
                    return;
                };
                if product.elements.len() != value.elements.len() {
                    self.diagnostics.push(Diagnostic::new(
                        product.syntax.span.clone(),
                        format!(
                            "product pattern has {} elements but value has {}",
                            product.elements.len(),
                            value.elements.len()
                        ),
                    ));
                    return;
                }
                for (pattern, element) in product.elements.iter().zip(&value.elements) {
                    self.check_match_pattern(module, pattern, &element.value_type);
                }
            }
            Pattern::Nominal(pattern) => {
                let Some(expected_id) = module.type_for_pattern(pattern.syntax.id) else {
                    return;
                };
                let representation = match value_type {
                    CheckedType::Ref(payload)
                        if module.builtin_type(expected_id) == Some(BuiltinType::Ref) =>
                    {
                        Some(payload.as_ref())
                    }
                    CheckedType::Sum(sum) => {
                        sum.alternatives
                            .iter()
                            .find_map(|alternative| match alternative {
                                CheckedType::Distinct {
                                    id, representation, ..
                                } if *id == expected_id => Some(representation.as_ref()),
                                _ => None,
                            })
                    }
                    CheckedType::Distinct {
                        id, representation, ..
                    } if *id == expected_id => Some(representation.as_ref()),
                    _ => None,
                };
                if let Some(representation) = representation {
                    self.check_match_pattern(module, &pattern.argument, representation);
                } else if *value_type != CheckedType::Error {
                    self.diagnostics.push(Diagnostic::new(
                        pattern.syntax.span.clone(),
                        format!(
                            "nominal pattern `{}` cannot match `{value_type}`",
                            pattern.name
                        ),
                    ));
                }
            }
        }
    }

    fn match_pattern_is_useful<'a>(
        module: &ResolvedModule,
        value_type: &CheckedType,
        previous: &[&'a Pattern],
        candidate: &'a Pattern,
    ) -> bool {
        let matrix = previous
            .iter()
            .map(|pattern| vec![CoveragePattern::Pattern(pattern)])
            .collect::<Vec<_>>();
        Self::coverage_is_useful(
            module,
            std::slice::from_ref(value_type),
            &matrix,
            &[CoveragePattern::Pattern(candidate)],
        )
    }

    fn coverage_is_useful<'a>(
        module: &ResolvedModule,
        types: &[CheckedType],
        matrix: &[Vec<CoveragePattern<'a>>],
        candidate: &[CoveragePattern<'a>],
    ) -> bool {
        if types.is_empty() {
            return !matrix.iter().any(Vec::is_empty);
        }
        let first = Self::canonical_coverage_pattern(candidate[0]);
        match &types[0] {
            CheckedType::Sum(sum) => {
                let alternatives = match first {
                    CoveragePattern::Any => (0..sum.alternatives.len()).collect::<Vec<_>>(),
                    CoveragePattern::Pattern(Pattern::Nominal(pattern)) => module
                        .type_for_pattern(pattern.syntax.id)
                        .and_then(|id| {
                            sum.alternatives.iter().position(
                                |alternative| matches!(alternative, CheckedType::Distinct { id: alternative_id, .. } if *alternative_id == id),
                            )
                        })
                        .into_iter()
                        .collect(),
                    _ => return false,
                };
                alternatives.into_iter().any(|index| {
                    let CheckedType::Distinct { representation, .. } = &sum.alternatives[index]
                    else {
                        return false;
                    };
                    let specialized_matrix = matrix
                        .iter()
                        .filter_map(|row| Self::specialize_sum_row(module, row, index, sum))
                        .collect::<Vec<_>>();
                    let Some(specialized_candidate) =
                        Self::specialize_sum_row(module, candidate, index, sum)
                    else {
                        return false;
                    };
                    let mut specialized_types = vec![representation.as_ref().clone()];
                    specialized_types.extend_from_slice(&types[1..]);
                    Self::coverage_is_useful(
                        module,
                        &specialized_types,
                        &specialized_matrix,
                        &specialized_candidate,
                    )
                })
            }
            CheckedType::Product(product) => {
                let Some(specialized_candidate) =
                    Self::specialize_product_row(candidate, product.elements.len())
                else {
                    return false;
                };
                let specialized_matrix = matrix
                    .iter()
                    .filter_map(|row| Self::specialize_product_row(row, product.elements.len()))
                    .collect::<Vec<_>>();
                let mut specialized_types = product
                    .elements
                    .iter()
                    .map(|element| element.value_type.clone())
                    .collect::<Vec<_>>();
                specialized_types.extend_from_slice(&types[1..]);
                Self::coverage_is_useful(
                    module,
                    &specialized_types,
                    &specialized_matrix,
                    &specialized_candidate,
                )
            }
            CheckedType::Distinct {
                id, representation, ..
            } => {
                let Some(specialized_candidate) =
                    Self::specialize_distinct_row(module, candidate, *id)
                else {
                    return false;
                };
                let specialized_matrix = matrix
                    .iter()
                    .filter_map(|row| Self::specialize_distinct_row(module, row, *id))
                    .collect::<Vec<_>>();
                let mut specialized_types = vec![representation.as_ref().clone()];
                specialized_types.extend_from_slice(&types[1..]);
                Self::coverage_is_useful(
                    module,
                    &specialized_types,
                    &specialized_matrix,
                    &specialized_candidate,
                )
            }
            CheckedType::Ref(payload) => {
                let Some(specialized_candidate) = Self::specialize_ref_row(module, candidate)
                else {
                    return false;
                };
                let specialized_matrix = matrix
                    .iter()
                    .filter_map(|row| Self::specialize_ref_row(module, row))
                    .collect::<Vec<_>>();
                let mut specialized_types = vec![payload.as_ref().clone()];
                specialized_types.extend_from_slice(&types[1..]);
                Self::coverage_is_useful(
                    module,
                    &specialized_types,
                    &specialized_matrix,
                    &specialized_candidate,
                )
            }
            _ => {
                if !matches!(first, CoveragePattern::Any) {
                    return false;
                }
                let specialized_matrix = matrix
                    .iter()
                    .filter(|row| {
                        matches!(
                            Self::canonical_coverage_pattern(row[0]),
                            CoveragePattern::Any
                        )
                    })
                    .map(|row| row[1..].to_vec())
                    .collect::<Vec<_>>();
                Self::coverage_is_useful(module, &types[1..], &specialized_matrix, &candidate[1..])
            }
        }
    }

    fn canonical_coverage_pattern(pattern: CoveragePattern<'_>) -> CoveragePattern<'_> {
        match pattern {
            CoveragePattern::Pattern(Pattern::Binding(_) | Pattern::Wildcard(_)) => {
                CoveragePattern::Any
            }
            CoveragePattern::Pattern(Pattern::Product(product)) if product.elements.len() == 1 => {
                Self::canonical_coverage_pattern(CoveragePattern::Pattern(&product.elements[0]))
            }
            other => other,
        }
    }

    fn specialize_sum_row<'a>(
        module: &ResolvedModule,
        row: &[CoveragePattern<'a>],
        index: usize,
        sum: &CheckedSumType,
    ) -> Option<Vec<CoveragePattern<'a>>> {
        let first = Self::canonical_coverage_pattern(row[0]);
        let selected_id = match &sum.alternatives[index] {
            CheckedType::Distinct { id, .. } => *id,
            _ => return None,
        };
        let head = match first {
            CoveragePattern::Any => CoveragePattern::Any,
            CoveragePattern::Pattern(Pattern::Nominal(pattern))
                if module.type_for_pattern(pattern.syntax.id) == Some(selected_id) =>
            {
                CoveragePattern::Pattern(&pattern.argument)
            }
            _ => return None,
        };
        let mut result = vec![head];
        result.extend_from_slice(&row[1..]);
        Some(result)
    }

    fn specialize_product_row<'a>(
        row: &[CoveragePattern<'a>],
        length: usize,
    ) -> Option<Vec<CoveragePattern<'a>>> {
        let first = Self::canonical_coverage_pattern(row[0]);
        let mut result = match first {
            CoveragePattern::Any => vec![CoveragePattern::Any; length],
            CoveragePattern::Pattern(Pattern::Product(product))
                if product.elements.len() == length =>
            {
                product
                    .elements
                    .iter()
                    .map(CoveragePattern::Pattern)
                    .collect()
            }
            _ => return None,
        };
        result.extend_from_slice(&row[1..]);
        Some(result)
    }

    fn specialize_distinct_row<'a>(
        module: &ResolvedModule,
        row: &[CoveragePattern<'a>],
        id: TypeId,
    ) -> Option<Vec<CoveragePattern<'a>>> {
        let first = Self::canonical_coverage_pattern(row[0]);
        let head = match first {
            CoveragePattern::Any => CoveragePattern::Any,
            CoveragePattern::Pattern(Pattern::Nominal(pattern))
                if module.type_for_pattern(pattern.syntax.id) == Some(id) =>
            {
                CoveragePattern::Pattern(&pattern.argument)
            }
            _ => return None,
        };
        let mut result = vec![head];
        result.extend_from_slice(&row[1..]);
        Some(result)
    }

    fn specialize_ref_row<'a>(
        module: &ResolvedModule,
        row: &[CoveragePattern<'a>],
    ) -> Option<Vec<CoveragePattern<'a>>> {
        let first = Self::canonical_coverage_pattern(row[0]);
        let head = match first {
            CoveragePattern::Any => CoveragePattern::Any,
            CoveragePattern::Pattern(Pattern::Nominal(pattern))
                if module
                    .type_for_pattern(pattern.syntax.id)
                    .is_some_and(|id| module.builtin_type(id) == Some(BuiltinType::Ref)) =>
            {
                CoveragePattern::Pattern(&pattern.argument)
            }
            _ => return None,
        };
        let mut result = vec![head];
        result.extend_from_slice(&row[1..]);
        Some(result)
    }

    fn finish_expression_type(
        &mut self,
        expression: &Expression,
        natural_type: CheckedType,
        expected: Option<&CheckedType>,
    ) -> CheckedType {
        let value_type = match expected {
            _ if self.did_return => natural_type,
            Some(CheckedType::Product(product)) if product.variadic => natural_type,
            Some(expected) => self.coerce_expression_type(
                expression.syntax().id,
                natural_type,
                expected,
                expression.syntax().span.clone(),
            ),
            None => natural_type,
        };
        self.expression_types
            .insert(expression.syntax().id, value_type.clone());
        value_type
    }

    fn coerce_expression_type(
        &mut self,
        syntax: SyntaxId,
        actual: CheckedType,
        expected: &CheckedType,
        span: Span,
    ) -> CheckedType {
        if let Some(merged) = merge_types(actual.clone(), expected.clone()) {
            return merged;
        }
        let allowed = match (&actual, expected) {
            (CheckedType::Distinct { .. }, CheckedType::Sum(sum)) => {
                sum.alternatives.contains(&actual)
            }
            (CheckedType::Sum(actual), CheckedType::Sum(expected)) => actual
                .alternatives
                .iter()
                .all(|alternative| expected.alternatives.contains(alternative)),
            _ => false,
        };
        if allowed {
            self.expression_coercions.insert(
                syntax,
                CheckedCoercion {
                    source: actual,
                    target: expected.clone(),
                },
            );
            expected.clone()
        } else {
            self.diagnostics.push(Diagnostic::new(
                span,
                format!("expected `{expected}`, found `{actual}`"),
            ));
            CheckedType::Error
        }
    }

    fn resolve_trait_method_use(
        &mut self,
        module: &ResolvedModule,
        syntax: SyntaxId,
        candidates: &[TraitMethodId],
        argument: Option<&CheckedType>,
        expected: Option<&CheckedType>,
        span: Span,
    ) -> CheckedType {
        let mut matches = Vec::new();
        let mut unavailable = false;
        for method in candidates {
            let Some(template) = self.trait_method_types.get(method).cloned() else {
                continue;
            };
            let CheckedType::Function(function) = &template else {
                continue;
            };
            let mut substitutions = HashMap::new();
            if let Some(argument) = argument
                && !infer_type_parameters(&function.parameter, argument, &mut substitutions)
            {
                continue;
            }
            if let Some(expected) = expected {
                let expected_template = if argument.is_some() {
                    function.result.as_ref()
                } else {
                    &template
                };
                if !infer_type_parameters(expected_template, expected, &mut substitutions) {
                    continue;
                }
            }
            let trait_id = module
                .trait_for_method(*method)
                .expect("trait method owner");
            let parameter = module.traits()[&trait_id].parameter;
            let Some(target) = substitutions.get(&parameter).cloned() else {
                continue;
            };
            if !self.trait_obligation_available(trait_id, &target) {
                unavailable = true;
                continue;
            }
            matches.push((*method, target, substitute_type(template, &substitutions)));
        }
        if matches.len() == 1 {
            let (method, target, value_type) = matches.pop().expect("one method match");
            self.trait_dispatches
                .insert(syntax, CheckedTraitDispatch { method, target });
            return value_type;
        }
        self.diagnostics.push(Diagnostic::new(
            span,
            if matches.len() > 1 {
                "ambiguous trait method; qualify the trait name"
            } else if unavailable {
                "no trait implementation or matching bound is available"
            } else {
                "could not infer the trait method's target type"
            },
        ));
        CheckedType::Error
    }

    fn trait_obligation_available(&self, trait_id: TraitId, target: &CheckedType) -> bool {
        if Some(trait_id) == self.copy_trait {
            let bounds = self
                .active_function_bounds
                .iter()
                .flatten()
                .cloned()
                .collect::<Vec<_>>();
            return is_copy_type(
                target,
                self.copy_trait,
                self.drop_trait,
                &self.trait_implementations,
                &bounds,
            );
        }
        if !contains_type_parameter(target) {
            return self.trait_implementations.iter().any(|implementation| {
                implementation.trait_id == trait_id && &implementation.target == target
            });
        }
        self.active_function_bounds.iter().rev().any(|bounds| {
            bounds
                .iter()
                .any(|bound| bound.trait_id == trait_id && &bound.argument == target)
        })
    }

    fn function_origin(
        &self,
        module: &ResolvedModule,
        expression: &Expression,
    ) -> Option<FunctionId> {
        match expression {
            Expression::Name(name) => module
                .symbol_for(name.syntax.id)
                .and_then(|symbol| self.function_symbols.get(&symbol).copied()),
            Expression::Access(access) => module
                .symbol_for(access.syntax.id)
                .and_then(|symbol| self.function_symbols.get(&symbol).copied()),
            Expression::Call(call) => self.function_origin(module, &call.callee),
            Expression::Infix(infix) => module
                .lowered_infix(infix.syntax.id)
                .and_then(|expression| self.function_origin(module, expression)),
            _ => None,
        }
    }

    fn check_function_bounds(
        &mut self,
        function_id: FunctionId,
        template: &CheckedType,
        instantiated: &CheckedType,
        span: Span,
    ) {
        let bounds = self
            .function_bounds
            .get(&function_id)
            .cloned()
            .unwrap_or_default();
        if bounds.is_empty() {
            return;
        }
        let mut substitutions = HashMap::new();
        if !infer_type_parameters(template, instantiated, &mut substitutions) {
            return;
        }
        for bound in bounds {
            let argument = substitute_type(bound.argument, &substitutions);
            if !self.trait_obligation_available(bound.trait_id, &argument) {
                self.diagnostics.push(Diagnostic::new(
                    span.clone(),
                    format!("trait bound is not satisfied for `{argument}`"),
                ));
            }
        }
    }

    fn instantiate_function_use(
        &mut self,
        value_type: CheckedType,
        argument: Option<&CheckedType>,
        expected: Option<&CheckedType>,
        span: Span,
    ) -> CheckedType {
        if !contains_type_parameter(&value_type) {
            return value_type;
        }
        let declared_parameters = type_parameter_ids(&value_type);
        let CheckedType::Function(function) = &value_type else {
            return value_type;
        };
        let mut substitutions = HashMap::new();
        if let Some(argument) = argument
            && !infer_type_parameters(&function.parameter, argument, &mut substitutions)
        {
            self.diagnostics.push(Diagnostic::new(
                span.clone(),
                format!(
                    "generic function parameter `{}` conflicts with argument `{argument}`",
                    function.parameter
                ),
            ));
            return CheckedType::Error;
        }
        if let Some(expected) = expected {
            let template = if argument.is_some() {
                function.result.as_ref()
            } else {
                &value_type
            };
            if !infer_type_parameters_for_expected(template, expected, &mut substitutions) {
                let polymorphic_recursion = declared_parameters.iter().any(|id| {
                    substitutions.get(id).is_some_and(|replacement| {
                        type_contains_parameter(replacement, *id)
                            && !matches!(replacement, CheckedType::Parameter { id: replacement_id, .. } if replacement_id == id)
                    })
                });
                self.diagnostics.push(Diagnostic::new(
                    span.clone(),
                    if polymorphic_recursion {
                        "polymorphic recursion is not supported"
                    } else {
                        "generic function conflicts with the expected type"
                    },
                ));
                return CheckedType::Error;
            }
        }
        let instantiated = substitute_type(value_type, &substitutions);
        if argument.is_some() || expected.is_some() {
            for id in declared_parameters {
                let Some(replacement) = substitutions.get(&id) else {
                    if matches!(instantiated, CheckedType::Function(_)) {
                        continue;
                    }
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "could not infer all compile-time parameters",
                    ));
                    return CheckedType::Error;
                };
                if type_contains_parameter(replacement, id)
                    && !matches!(replacement, CheckedType::Parameter { id: replacement_id, .. } if *replacement_id == id)
                {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "polymorphic recursion is not supported",
                    ));
                    return CheckedType::Error;
                }
            }
        }
        instantiated
    }

    fn check_call_argument(&mut self, actual: CheckedType, expected: &CheckedType, span: Span) {
        if let CheckedType::Product(expected_product) = expected
            && expected_product.variadic
        {
            let actual_elements = match actual {
                CheckedType::Product(product) => product.elements,
                value_type => vec![CheckedTypeElement {
                    name: None,
                    value_type,
                }],
            };
            if actual_elements.len() < expected_product.elements.len() {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    format!(
                        "expected at least {} arguments",
                        expected_product.elements.len()
                    ),
                ));
                return;
            }
            for (actual, expected) in actual_elements.into_iter().zip(&expected_product.elements) {
                self.require_compatible(
                    actual.value_type,
                    expected.value_type.clone(),
                    span.clone(),
                );
            }
            return;
        }
        self.require_compatible(actual, expected.clone(), span);
    }

    fn require_compatible(
        &mut self,
        actual: CheckedType,
        expected: CheckedType,
        span: Span,
    ) -> CheckedType {
        match merge_types(actual.clone(), expected.clone()) {
            Some(value_type) => value_type,
            None => {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    format!("expected `{expected}`, found `{actual}`"),
                ));
                CheckedType::Error
            }
        }
    }

    fn resolve_source_type(&mut self, module: &ResolvedModule, source_type: &Type) -> CheckedType {
        match source_type {
            Type::Inferred(_) => CheckedType::Inferred,
            Type::Named(named) => {
                if let Some(id) = module.type_parameter_for(named.syntax.id) {
                    CheckedType::Parameter {
                        id,
                        name: named.name.clone(),
                    }
                } else {
                    self.resolve_named_type(module, named)
                }
            }
            Type::Product(product) => {
                let product = self.resolve_product_type(module, product);
                normalize_product_type(product.elements, product.variadic)
            }
            Type::Sum(sum) => {
                let alternatives = sum
                    .alternatives
                    .iter()
                    .map(|alternative| self.resolve_source_type(module, alternative))
                    .collect();
                self.normalize_sum_type(alternatives, sum.syntax.span.clone())
            }
            Type::Function(function) => CheckedType::Function(CheckedFunctionType {
                parameter: Box::new(self.resolve_source_type(module, &function.parameter)),
                result: Box::new(self.resolve_source_type(module, &function.result)),
            }),
            Type::Application(application) => {
                let callee = self.resolve_source_type(module, &application.callee);
                let argument = self.resolve_source_type(module, &application.argument);
                self.apply_type_argument(module, callee, argument, application.syntax.span.clone())
            }
        }
    }

    fn normalize_sum_type(&mut self, alternatives: Vec<CheckedType>, span: Span) -> CheckedType {
        let mut flattened = Vec::new();
        for alternative in alternatives {
            match alternative {
                CheckedType::Sum(sum) => flattened.extend(sum.alternatives),
                other => flattened.push(other),
            }
        }
        flattened.retain(|alternative| *alternative != CheckedType::Error);
        flattened.sort_by_key(checked_type_sort_key);
        flattened.dedup();
        let mut heads = HashMap::<TypeId, CheckedType>::new();
        for alternative in &flattened {
            let CheckedType::Distinct { id, .. } = alternative else {
                self.diagnostics.push(Diagnostic::new(
                    span.clone(),
                    format!("sum alternative `{alternative}` is not a represented nominal type"),
                ));
                return CheckedType::Error;
            };
            if let Some(previous) = heads.insert(*id, alternative.clone())
                && previous != *alternative
            {
                self.diagnostics.push(Diagnostic::new(
                    span.clone(),
                    format!("sum contains multiple applications of the same nominal type: `{previous}` and `{alternative}`"),
                ));
                return CheckedType::Error;
            }
        }
        match flattened.len() {
            0 => CheckedType::Error,
            1 => flattened.pop().expect("one alternative"),
            _ => CheckedType::Sum(CheckedSumType {
                alternatives: flattened,
            }),
        }
    }

    fn apply_type_argument(
        &mut self,
        module: &ResolvedModule,
        callee: CheckedType,
        argument: CheckedType,
        span: Span,
    ) -> CheckedType {
        let CheckedType::TypeConstructor {
            id,
            name,
            mut arguments,
        } = callee
        else {
            self.diagnostics.push(Diagnostic::new(
                span,
                format!("type `{callee}` does not accept compile-time arguments"),
            ));
            return CheckedType::Error;
        };
        arguments.push(argument);
        let declaration = &self.type_declarations[&id];
        if arguments.len() < declaration.type_parameters.len() {
            return CheckedType::TypeConstructor {
                id,
                name,
                arguments,
            };
        }
        if arguments.len() > declaration.type_parameters.len() {
            self.diagnostics.push(Diagnostic::new(
                span,
                format!("too many compile-time arguments for `{name}`"),
            ));
            return CheckedType::Error;
        }
        self.instantiate_type_declaration(module, id, arguments)
    }

    fn instantiate_type_declaration(
        &mut self,
        module: &ResolvedModule,
        id: TypeId,
        arguments: Vec<CheckedType>,
    ) -> CheckedType {
        let declaration = self.type_declarations[&id].clone();
        let display_name = module.type_name(id).unwrap_or(&declaration.name).to_owned();
        let mut substitutions = HashMap::new();
        for (pattern, argument) in declaration.type_parameters.iter().zip(&arguments) {
            if !self.bind_type_argument(module, pattern, argument, &mut substitutions) {
                return CheckedType::Error;
            }
        }
        if module.builtin_type(id) == Some(BuiltinType::CPointer) {
            return CheckedType::CPointer {
                pointee: Box::new(arguments[0].clone()),
            };
        }
        if module.builtin_type(id) == Some(BuiltinType::Ref) {
            return CheckedType::Ref(Box::new(arguments[0].clone()));
        }
        if declaration.kind == TypeDeclarationKind::Opaque {
            return CheckedType::Opaque {
                id,
                name: display_name,
                arguments,
            };
        }
        if declaration.kind == TypeDeclarationKind::Singleton {
            return CheckedType::Distinct {
                id,
                name: display_name,
                arguments,
                representation: Box::new(CheckedType::empty_product()),
            };
        }
        if !self.resolving_named_types.insert(id) {
            self.diagnostics.push(Diagnostic::new(
                declaration.syntax.span.clone(),
                format!("cyclic type definition involving `{display_name}`"),
            ));
            return CheckedType::Error;
        }
        let template = self.resolve_source_type(
            module,
            declaration.underlying.as_ref().expect("represented type"),
        );
        self.resolving_named_types.remove(&id);
        let representation = substitute_type(template, &substitutions);
        match declaration.kind {
            TypeDeclarationKind::Alias => representation,
            TypeDeclarationKind::Distinct => CheckedType::Distinct {
                id,
                name: display_name,
                arguments,
                representation: Box::new(representation),
            },
            TypeDeclarationKind::Singleton => unreachable!(),
            TypeDeclarationKind::Opaque => unreachable!(),
        }
    }

    fn bind_type_argument(
        &mut self,
        module: &ResolvedModule,
        pattern: &TypeParameterPattern,
        argument: &CheckedType,
        substitutions: &mut HashMap<TypeParameterId, CheckedType>,
    ) -> bool {
        match pattern {
            TypeParameterPattern::Binding(binding) => {
                let Some(id) = module.type_parameter_for(binding.syntax.id) else {
                    return false;
                };
                substitutions.insert(id, argument.clone());
                true
            }
            TypeParameterPattern::Product(product) => {
                if product.elements.len() == 1 {
                    return self.bind_type_argument(
                        module,
                        &product.elements[0],
                        argument,
                        substitutions,
                    );
                }
                let CheckedType::Product(argument) = argument else {
                    self.diagnostics.push(Diagnostic::new(
                        product.syntax.span.clone(),
                        "compile-time product parameter requires a product type argument",
                    ));
                    return false;
                };
                if product.elements.len() != argument.elements.len() {
                    self.diagnostics.push(Diagnostic::new(
                        product.syntax.span.clone(),
                        "compile-time product argument has the wrong number of elements",
                    ));
                    return false;
                }
                product
                    .elements
                    .iter()
                    .zip(&argument.elements)
                    .all(|(pattern, argument)| {
                        self.bind_type_argument(
                            module,
                            pattern,
                            &argument.value_type,
                            substitutions,
                        )
                    })
            }
        }
    }

    fn resolve_product_type(
        &mut self,
        module: &ResolvedModule,
        product: &ProductType,
    ) -> CheckedProductType {
        CheckedProductType {
            elements: product
                .elements
                .iter()
                .map(|element| CheckedTypeElement {
                    name: element.name.clone(),
                    value_type: self.resolve_source_type(module, &element.ty),
                })
                .collect(),
            variadic: product.variadic,
        }
    }

    fn resolve_named_type(
        &mut self,
        module: &ResolvedModule,
        named: &crate::NamedType,
    ) -> CheckedType {
        if named.name == "int" {
            return CheckedType::I32;
        }
        let Some(id) = module.type_for(named.syntax.id) else {
            return CheckedType::Error;
        };
        if let Some(builtin) = module.builtin_type(id) {
            return match builtin {
                BuiltinType::Integer(integer) => CheckedType::integer(integer),
                BuiltinType::String => CheckedType::String,
                BuiltinType::Ref => CheckedType::TypeConstructor {
                    id,
                    name: "Ref".to_owned(),
                    arguments: Vec::new(),
                },
                BuiltinType::CChar => CheckedType::CChar,
                BuiltinType::CString => CheckedType::CString,
                BuiltinType::CPointer => CheckedType::TypeConstructor {
                    id,
                    name: "CPointer".to_owned(),
                    arguments: Vec::new(),
                },
            };
        }
        if let Some(value_type) = self.resolved_named_types.get(&id) {
            return value_type.clone();
        }
        let declaration = self.type_declarations[&id].clone();
        let display_name = module.type_name(id).unwrap_or(&declaration.name).to_owned();
        if !declaration.type_parameters.is_empty() {
            return CheckedType::TypeConstructor {
                id,
                name: display_name,
                arguments: Vec::new(),
            };
        }
        if declaration.kind == TypeDeclarationKind::Opaque {
            let value_type = CheckedType::Opaque {
                id,
                name: display_name,
                arguments: Vec::new(),
            };
            self.resolved_named_types.insert(id, value_type.clone());
            return value_type;
        }
        if declaration.kind == TypeDeclarationKind::Singleton {
            let value_type = CheckedType::Distinct {
                id,
                name: display_name,
                arguments: Vec::new(),
                representation: Box::new(CheckedType::empty_product()),
            };
            self.resolved_named_types.insert(id, value_type.clone());
            return value_type;
        }
        if !self.resolving_named_types.insert(id) {
            self.diagnostics.push(Diagnostic::new(
                declaration.syntax.span.clone(),
                format!("cyclic type definition involving `{display_name}`"),
            ));
            return CheckedType::Error;
        }
        let representation = self.resolve_source_type(
            module,
            declaration
                .underlying
                .as_ref()
                .expect("non-opaque type declaration has an underlying type"),
        );
        self.resolving_named_types.remove(&id);
        let value_type = match declaration.kind {
            TypeDeclarationKind::Alias => representation,
            TypeDeclarationKind::Distinct => CheckedType::Distinct {
                id,
                name: display_name,
                arguments: Vec::new(),
                representation: Box::new(representation),
            },
            TypeDeclarationKind::Singleton => unreachable!(),
            TypeDeclarationKind::Opaque => unreachable!(),
        };
        self.resolved_named_types.insert(id, value_type.clone());
        value_type
    }
}

fn merge_types(actual: CheckedType, expected: CheckedType) -> Option<CheckedType> {
    match (actual, expected) {
        (CheckedType::Error, _) | (_, CheckedType::Error) => Some(CheckedType::Error),
        (CheckedType::Inferred, value_type) | (value_type, CheckedType::Inferred) => {
            Some(value_type)
        }
        (actual, expected)
            if actual.integer_type().is_some()
                && actual.integer_type() == expected.integer_type() =>
        {
            Some(actual)
        }
        (CheckedType::String, CheckedType::String) => Some(CheckedType::String),
        (CheckedType::CString, CheckedType::CString) => Some(CheckedType::CString),
        (CheckedType::CChar, CheckedType::CChar) => Some(CheckedType::CChar),
        (
            CheckedType::Parameter { id: actual, name },
            CheckedType::Parameter { id: expected, .. },
        ) if actual == expected => Some(CheckedType::Parameter { id: actual, name }),
        (
            CheckedType::Opaque {
                id: actual_id,
                name: actual_name,
                arguments: actual_arguments,
            },
            CheckedType::Opaque {
                id: expected_id,
                name: _,
                arguments: expected_arguments,
            },
        ) if actual_id == expected_id && actual_arguments.len() == expected_arguments.len() => {
            Some(CheckedType::Opaque {
                id: actual_id,
                name: actual_name,
                arguments: actual_arguments
                    .into_iter()
                    .zip(expected_arguments)
                    .map(|(actual, expected)| merge_types(actual, expected))
                    .collect::<Option<Vec<_>>>()?,
            })
        }
        (CheckedType::CString, CheckedType::CPointer { pointee })
            if *pointee == CheckedType::CChar =>
        {
            Some(CheckedType::CString)
        }
        (
            CheckedType::CPointer { pointee: actual },
            CheckedType::CPointer { pointee: expected },
        ) => merge_types(*actual, *expected).map(|pointee| CheckedType::CPointer {
            pointee: Box::new(pointee),
        }),
        (CheckedType::Ref(actual), CheckedType::Ref(expected)) => {
            merge_types(*actual, *expected).map(|value| CheckedType::Ref(Box::new(value)))
        }
        (CheckedType::Product(actual), CheckedType::Product(expected))
            if actual.variadic == expected.variadic
                && actual.elements.len() == expected.elements.len() =>
        {
            let elements = actual
                .elements
                .into_iter()
                .zip(expected.elements)
                .map(|(actual, expected)| {
                    merge_types(actual.value_type, expected.value_type).map(|value_type| {
                        CheckedTypeElement {
                            name: expected.name.or(actual.name),
                            value_type,
                        }
                    })
                })
                .collect::<Option<Vec<_>>>()?;
            Some(normalize_product_type(elements, actual.variadic))
        }
        (CheckedType::Function(actual), CheckedType::Function(expected)) => {
            Some(CheckedType::Function(CheckedFunctionType {
                parameter: Box::new(merge_types(*actual.parameter, *expected.parameter)?),
                result: Box::new(merge_types(*actual.result, *expected.result)?),
            }))
        }
        (CheckedType::Sum(actual), CheckedType::Sum(expected))
            if actual.alternatives.len() == expected.alternatives.len() =>
        {
            let alternatives = actual
                .alternatives
                .into_iter()
                .zip(expected.alternatives)
                .map(|(actual, expected)| merge_types(actual, expected))
                .collect::<Option<Vec<_>>>()?;
            Some(CheckedType::Sum(CheckedSumType { alternatives }))
        }
        (
            CheckedType::Distinct {
                id: actual_id,
                name: actual_name,
                arguments: actual_arguments,
                representation: actual_representation,
            },
            CheckedType::Distinct {
                id: expected_id,
                name: _,
                arguments: expected_arguments,
                representation: expected_representation,
            },
        ) if actual_id == expected_id && actual_arguments == expected_arguments => {
            Some(CheckedType::Distinct {
                id: actual_id,
                name: actual_name,
                arguments: actual_arguments,
                representation: Box::new(merge_types(
                    *actual_representation,
                    *expected_representation,
                )?),
            })
        }
        _ => None,
    }
}

fn can_coerce_type(actual: &CheckedType, expected: &CheckedType) -> bool {
    if merge_types(actual.clone(), expected.clone()).is_some() {
        return true;
    }
    match (actual, expected) {
        (CheckedType::Distinct { .. }, CheckedType::Sum(sum)) => sum.alternatives.contains(actual),
        (CheckedType::Sum(actual), CheckedType::Sum(expected)) => actual
            .alternatives
            .iter()
            .all(|alternative| expected.alternatives.contains(alternative)),
        _ => false,
    }
}

pub(crate) fn substitute_type(
    value_type: CheckedType,
    substitutions: &HashMap<TypeParameterId, CheckedType>,
) -> CheckedType {
    match value_type {
        CheckedType::Parameter { id, name } => substitutions
            .get(&id)
            .cloned()
            .unwrap_or(CheckedType::Parameter { id, name }),
        CheckedType::CPointer { pointee } => CheckedType::CPointer {
            pointee: Box::new(substitute_type(*pointee, substitutions)),
        },
        CheckedType::Ref(value) => {
            CheckedType::Ref(Box::new(substitute_type(*value, substitutions)))
        }
        CheckedType::Opaque {
            id,
            name,
            arguments,
        } => CheckedType::Opaque {
            id,
            name,
            arguments: arguments
                .into_iter()
                .map(|argument| substitute_type(argument, substitutions))
                .collect(),
        },
        CheckedType::Product(product) => CheckedType::Product(CheckedProductType {
            elements: product
                .elements
                .into_iter()
                .map(|element| CheckedTypeElement {
                    name: element.name,
                    value_type: substitute_type(element.value_type, substitutions),
                })
                .collect(),
            variadic: product.variadic,
        }),
        CheckedType::Function(function) => CheckedType::Function(CheckedFunctionType {
            parameter: Box::new(substitute_type(*function.parameter, substitutions)),
            result: Box::new(substitute_type(*function.result, substitutions)),
        }),
        CheckedType::Sum(sum) => CheckedType::Sum(CheckedSumType {
            alternatives: sum
                .alternatives
                .into_iter()
                .map(|alternative| substitute_type(alternative, substitutions))
                .collect(),
        }),
        CheckedType::Distinct {
            id,
            name,
            arguments,
            representation,
        } => CheckedType::Distinct {
            id,
            name,
            arguments: arguments
                .into_iter()
                .map(|argument| substitute_type(argument, substitutions))
                .collect(),
            representation: Box::new(substitute_type(*representation, substitutions)),
        },
        CheckedType::TypeConstructor {
            id,
            name,
            arguments,
        } => CheckedType::TypeConstructor {
            id,
            name,
            arguments: arguments
                .into_iter()
                .map(|argument| substitute_type(argument, substitutions))
                .collect(),
        },
        other => other,
    }
}

pub(crate) fn contains_type_parameter(value_type: &CheckedType) -> bool {
    match value_type {
        CheckedType::Parameter { .. } => true,
        CheckedType::CPointer { pointee } => contains_type_parameter(pointee),
        CheckedType::Ref(value) => contains_type_parameter(value),
        CheckedType::Opaque { arguments, .. } => arguments.iter().any(contains_type_parameter),
        CheckedType::Product(product) => product
            .elements
            .iter()
            .any(|element| contains_type_parameter(&element.value_type)),
        CheckedType::Function(function) => {
            contains_type_parameter(&function.parameter)
                || contains_type_parameter(&function.result)
        }
        CheckedType::Sum(sum) => sum.alternatives.iter().any(contains_type_parameter),
        CheckedType::Distinct {
            arguments,
            representation,
            ..
        } => {
            arguments.iter().any(contains_type_parameter) || contains_type_parameter(representation)
        }
        CheckedType::TypeConstructor { arguments, .. } => {
            arguments.iter().any(contains_type_parameter)
        }
        _ => false,
    }
}

fn contains_type_parameter_id(value_type: &CheckedType, expected: TypeParameterId) -> bool {
    type_parameter_ids(value_type).contains(&expected)
}

fn contains_inferred_type(value_type: &CheckedType) -> bool {
    match value_type {
        CheckedType::Inferred | CheckedType::TypeConstructor { .. } => true,
        CheckedType::CPointer { pointee } => contains_inferred_type(pointee),
        CheckedType::Ref(value) => contains_inferred_type(value),
        CheckedType::Opaque { arguments, .. } => arguments.iter().any(contains_inferred_type),
        CheckedType::Product(product) => product
            .elements
            .iter()
            .any(|element| contains_inferred_type(&element.value_type)),
        CheckedType::Function(function) => {
            contains_inferred_type(&function.parameter) || contains_inferred_type(&function.result)
        }
        CheckedType::Sum(sum) => sum.alternatives.iter().any(contains_inferred_type),
        CheckedType::Distinct {
            arguments,
            representation,
            ..
        } => arguments.iter().any(contains_inferred_type) || contains_inferred_type(representation),
        _ => false,
    }
}

fn type_parameter_ids(value_type: &CheckedType) -> HashSet<TypeParameterId> {
    fn collect(value_type: &CheckedType, ids: &mut HashSet<TypeParameterId>) {
        match value_type {
            CheckedType::Parameter { id, .. } => {
                ids.insert(*id);
            }
            CheckedType::CPointer { pointee } => collect(pointee, ids),
            CheckedType::Ref(value) => collect(value, ids),
            CheckedType::Opaque { arguments, .. } => {
                for argument in arguments {
                    collect(argument, ids);
                }
            }
            CheckedType::Product(product) => {
                for element in &product.elements {
                    collect(&element.value_type, ids);
                }
            }
            CheckedType::Function(function) => {
                collect(&function.parameter, ids);
                collect(&function.result, ids);
            }
            CheckedType::Sum(sum) => {
                for alternative in &sum.alternatives {
                    collect(alternative, ids);
                }
            }
            CheckedType::Distinct {
                arguments,
                representation,
                ..
            } => {
                for argument in arguments {
                    collect(argument, ids);
                }
                collect(representation, ids);
            }
            CheckedType::TypeConstructor { arguments, .. } => {
                for argument in arguments {
                    collect(argument, ids);
                }
            }
            _ => {}
        }
    }
    let mut ids = HashSet::new();
    collect(value_type, &mut ids);
    ids
}

fn type_contains_parameter(value_type: &CheckedType, expected: TypeParameterId) -> bool {
    type_parameter_ids(value_type).contains(&expected)
}

pub(crate) fn infer_type_parameters(
    template: &CheckedType,
    actual: &CheckedType,
    substitutions: &mut HashMap<TypeParameterId, CheckedType>,
) -> bool {
    match template {
        CheckedType::Parameter { id, .. } => match substitutions.get(id) {
            Some(existing) => existing == actual,
            None => {
                substitutions.insert(*id, actual.clone());
                true
            }
        },
        CheckedType::CPointer { pointee } => {
            matches!(actual, CheckedType::CPointer { pointee: actual_pointee }
            if infer_type_parameters(pointee, actual_pointee, substitutions))
        }
        CheckedType::Ref(value) => {
            matches!(actual, CheckedType::Ref(actual_value)
                if infer_type_parameters(value, actual_value, substitutions))
        }
        CheckedType::Opaque { id, arguments, .. } => {
            let CheckedType::Opaque {
                id: actual_id,
                arguments: actual_arguments,
                ..
            } = actual
            else {
                return false;
            };
            id == actual_id
                && arguments.len() == actual_arguments.len()
                && arguments
                    .iter()
                    .zip(actual_arguments)
                    .all(|(template, actual)| {
                        infer_type_parameters(template, actual, substitutions)
                    })
        }
        CheckedType::Product(template) => {
            let CheckedType::Product(actual) = actual else {
                return false;
            };
            template.variadic == actual.variadic
                && template.elements.len() == actual.elements.len()
                && template
                    .elements
                    .iter()
                    .zip(&actual.elements)
                    .all(|(template, actual)| {
                        infer_type_parameters(
                            &template.value_type,
                            &actual.value_type,
                            substitutions,
                        )
                    })
        }
        CheckedType::Function(template) => {
            let CheckedType::Function(actual) = actual else {
                return false;
            };
            infer_type_parameters(&template.parameter, &actual.parameter, substitutions)
                && infer_type_parameters(&template.result, &actual.result, substitutions)
        }
        CheckedType::Sum(template) => {
            let CheckedType::Sum(actual) = actual else {
                return false;
            };
            template.alternatives.len() == actual.alternatives.len()
                && template.alternatives.iter().zip(&actual.alternatives).all(
                    |(template, actual)| infer_type_parameters(template, actual, substitutions),
                )
        }
        CheckedType::Distinct { id, arguments, .. } => {
            let CheckedType::Distinct {
                id: actual_id,
                arguments: actual_arguments,
                ..
            } = actual
            else {
                return false;
            };
            id == actual_id
                && arguments.len() == actual_arguments.len()
                && arguments
                    .iter()
                    .zip(actual_arguments)
                    .all(|(template, actual)| {
                        infer_type_parameters(template, actual, substitutions)
                    })
        }
        _ => template == actual || *actual == CheckedType::Inferred,
    }
}

fn infer_type_parameters_for_expected(
    template: &CheckedType,
    expected: &CheckedType,
    substitutions: &mut HashMap<TypeParameterId, CheckedType>,
) -> bool {
    if let (CheckedType::Function(template), CheckedType::Function(expected)) = (template, expected)
    {
        return infer_type_parameters(&template.parameter, &expected.parameter, substitutions)
            && infer_type_parameters_for_expected(
                &template.result,
                &expected.result,
                substitutions,
            );
    }
    if !matches!(template, CheckedType::Sum(_))
        && let CheckedType::Sum(sum) = expected
    {
        let matches = sum
            .alternatives
            .iter()
            .filter_map(|alternative| {
                let mut candidate = substitutions.clone();
                infer_type_parameters(template, alternative, &mut candidate).then_some(candidate)
            })
            .collect::<Vec<_>>();
        if let [candidate] = matches.as_slice() {
            *substitutions = candidate.clone();
            return true;
        }
        return false;
    }
    infer_type_parameters(template, expected, substitutions)
}

fn normalize_product_type(elements: Vec<CheckedTypeElement>, variadic: bool) -> CheckedType {
    if !variadic && elements.len() == 1 {
        elements.into_iter().next().unwrap().value_type
    } else {
        CheckedType::Product(CheckedProductType { elements, variadic })
    }
}

fn checked_type_sort_key(value_type: &CheckedType) -> String {
    match value_type {
        CheckedType::Distinct { id, arguments, .. } => {
            format!("{:020}:{arguments:?}", id.0)
        }
        other => format!("{other:?}"),
    }
}

fn checked_type_contains_sum(value_type: &CheckedType) -> bool {
    match value_type {
        CheckedType::Sum(_) => true,
        CheckedType::Product(product) => product
            .elements
            .iter()
            .any(|element| checked_type_contains_sum(&element.value_type)),
        CheckedType::Function(function) => {
            checked_type_contains_sum(&function.parameter)
                || checked_type_contains_sum(&function.result)
        }
        CheckedType::CPointer { pointee } => checked_type_contains_sum(pointee),
        CheckedType::Ref(value) => checked_type_contains_sum(value),
        CheckedType::Distinct {
            arguments,
            representation,
            ..
        } => {
            arguments.iter().any(checked_type_contains_sum)
                || checked_type_contains_sum(representation)
        }
        CheckedType::Opaque { arguments, .. } | CheckedType::TypeConstructor { arguments, .. } => {
            arguments.iter().any(checked_type_contains_sum)
        }
        _ => false,
    }
}

fn checked_type_contains_cstring(value_type: &CheckedType) -> bool {
    match value_type {
        CheckedType::CString => true,
        CheckedType::Product(product) => product
            .elements
            .iter()
            .any(|element| checked_type_contains_cstring(&element.value_type)),
        CheckedType::Sum(sum) => sum.alternatives.iter().any(checked_type_contains_cstring),
        CheckedType::Distinct { representation, .. } => {
            checked_type_contains_cstring(representation)
        }
        _ => false,
    }
}

fn has_drop_implementation(
    value_type: &CheckedType,
    drop_trait: Option<TraitId>,
    implementations: &[CheckedTraitImplementation],
) -> bool {
    drop_trait.is_some_and(|drop_trait| {
        implementations.iter().any(|implementation| {
            implementation.trait_id == drop_trait && &implementation.target == value_type
        })
    })
}

fn is_copy_type(
    value_type: &CheckedType,
    copy_trait: Option<TraitId>,
    drop_trait: Option<TraitId>,
    implementations: &[CheckedTraitImplementation],
    bounds: &[CheckedTraitBound],
) -> bool {
    if has_drop_implementation(value_type, drop_trait, implementations) {
        return false;
    }
    match value_type {
        CheckedType::Inferred | CheckedType::Error => true,
        CheckedType::I32
        | CheckedType::I8
        | CheckedType::I16
        | CheckedType::I64
        | CheckedType::U8
        | CheckedType::U16
        | CheckedType::U32
        | CheckedType::U64
        | CheckedType::ISize
        | CheckedType::USize
        | CheckedType::String
        | CheckedType::CChar
        | CheckedType::CPointer { .. }
        | CheckedType::Ref(_)
        | CheckedType::Function(_) => true,
        CheckedType::CString => false,
        CheckedType::Parameter { .. } => copy_trait.is_some_and(|copy_trait| {
            bounds
                .iter()
                .any(|bound| bound.trait_id == copy_trait && &bound.argument == value_type)
        }),
        CheckedType::Product(product) => product.elements.iter().all(|element| {
            is_copy_type(
                &element.value_type,
                copy_trait,
                drop_trait,
                implementations,
                bounds,
            )
        }),
        CheckedType::Sum(sum) => sum.alternatives.iter().all(|alternative| {
            is_copy_type(alternative, copy_trait, drop_trait, implementations, bounds)
        }),
        CheckedType::Distinct { representation, .. } => is_copy_type(
            representation,
            copy_trait,
            drop_trait,
            implementations,
            bounds,
        ),
        CheckedType::Opaque { .. } | CheckedType::TypeConstructor { .. } => false,
    }
}

fn type_needs_drop(
    value_type: &CheckedType,
    drop_trait: Option<TraitId>,
    implementations: &[CheckedTraitImplementation],
) -> bool {
    if has_drop_implementation(value_type, drop_trait, implementations) {
        return true;
    }
    match value_type {
        CheckedType::CString => true,
        CheckedType::Product(product) => product
            .elements
            .iter()
            .any(|element| type_needs_drop(&element.value_type, drop_trait, implementations)),
        CheckedType::Sum(sum) => sum
            .alternatives
            .iter()
            .any(|alternative| type_needs_drop(alternative, drop_trait, implementations)),
        CheckedType::Distinct { representation, .. } => {
            type_needs_drop(representation, drop_trait, implementations)
        }
        _ => false,
    }
}
