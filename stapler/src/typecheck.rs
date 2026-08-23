use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::{
    Accessor, Binding, BuiltinType, Diagnostic, Expression, FloatType, FunctionId,
    IntegerType, Item, Module, ModuleId, Pattern, PatternBindingKind, ProductExpression,
    ProductType, ResolvedFunction, ResolvedModule, Span, SymbolId, SyntaxId, TraitId,
    TraitMethodId, Type, TypeDeclaration, TypeDeclarationKind, TypeId, TypeParameterId,
    TypeParameterPattern,
};

const MAX_PRODUCT_ARITY: usize = 65_535;

enum PlaceIssue {
    /// The root binding is not declared `mut` and cannot be reassigned.
    NotReassignable(String),
    /// The root binding is not declared `mut` and cannot be written through.
    NotMutable(String),
    /// The target is not a place at all (e.g. a temporary).
    NotAPlace,
}

fn place_expression_name(expression: &Expression) -> String {
    match expression {
        Expression::Name(name) => name.name.clone(),
        Expression::Access(access) => match &access.accessor {
            Accessor::Name(name) => name.clone(),
            Accessor::Index(index) => index.clone(),
            Accessor::Method(name) => name.clone(),
        },
        _ => "value".to_string(),
    }
}

fn source_type_id(module: &ResolvedModule, ty: &Type) -> Option<TypeId> {
    match ty {
        Type::Named(named) => module.type_for(named.syntax.id),
        Type::Application(application) => source_type_id(module, &application.callee),
        _ => None,
    }
}

fn checked_type_id(ty: &CheckedType) -> Option<TypeId> {
    match ty {
        CheckedType::TypeConstructor { id, .. }
        | CheckedType::Opaque { id, .. }
        | CheckedType::Distinct { id, .. } => Some(*id),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedTraitBound {
    pub trait_id: TraitId,
    pub arguments: Vec<CheckedType>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedSubtypeBound {
    pub parameter: TypeParameterId,
    pub supertype: CheckedType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedDefaultBound {
    pub parameter: TypeParameterId,
    pub default: CheckedType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedTraitDispatch {
    pub method: TraitMethodId,
    pub arguments: Vec<CheckedType>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StructuralTraitMethod {
    Index,
    MutateIndex,
    IntoIterator,
    Iterator,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CheckedFunctionalDependency {
    determinants: Vec<TypeParameterId>,
    dependent: TypeParameterId,
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

/// The resolved `Bool` type for one `&&`/`||` expression, recorded so
/// codegen can extract the sum tag without re-resolving the operator's
/// synthesized `bool_type` annotation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedLogical {
    pub bool_type: CheckedType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedAccess {
    pub index: usize,
    pub dereference: Option<CheckedType>,
    pub erased: bool,
    pub scalar: bool,
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
struct LoopCheckContext {
    expected: Option<CheckedType>,
    breaks: Vec<ReturnContribution>,
}

#[derive(Debug, Clone)]
struct CheckedTraitImplementation {
    span: Span,
    trait_id: TraitId,
    parameters: HashSet<TypeParameterId>,
    arguments: Vec<CheckedType>,
    bounds: Vec<CheckedTraitBound>,
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
    F32,
    F64,
    String,
    StringLiteralSet(Vec<String>),
    Ref(Box<CheckedType>),
    Buffer(Box<CheckedType>),
    ErasedProduct(Box<CheckedType>),
    CString,
    CChar,
    Parameter {
        id: TypeParameterId,
        name: String,
        sized: bool,
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

fn expected_string_representation() -> CheckedType {
    CheckedType::Ref(Box::new(CheckedType::ErasedProduct(Box::new(
        CheckedType::U8,
    ))))
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

impl CheckedProductType {
    pub fn homogeneous_element(&self) -> Option<&CheckedType> {
        let first = &self.elements.first()?.value_type;
        self.elements
            .iter()
            .all(|element| element.value_type == *first)
            .then_some(first)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedSumType {
    pub alternatives: Vec<CheckedType>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedFunctionType {
    pub parameter: Box<CheckedType>,
    pub resources: CheckedResourceSet,
    pub result: Box<CheckedType>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedResource {
    pub value_type: CheckedType,
}

/// A parameter a function may write into: either the whole parameter value
/// or one positional element of a product parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckedMutation {
    Whole,
    Element(usize),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckedResourceSet {
    pub resources: Vec<CheckedResource>,
    pub mutations: Vec<CheckedMutation>,
}

impl CheckedResourceSet {
    pub fn canonical(resources: Vec<CheckedResource>) -> Self {
        Self::canonical_with_mutations(resources, Vec::new())
    }

    pub fn canonical_with_mutations(
        mut resources: Vec<CheckedResource>,
        mut mutations: Vec<CheckedMutation>,
    ) -> Self {
        resources.sort_by(|left, right| {
            format!("{:?}", left.value_type).cmp(&format!("{:?}", right.value_type))
        });
        resources.dedup();
        mutations.sort_by_key(|mutation| match mutation {
            CheckedMutation::Whole => (0, 0),
            CheckedMutation::Element(index) => (1, *index),
        });
        mutations.dedup();
        Self {
            resources,
            mutations,
        }
    }

    pub fn union(&self, other: &Self) -> Self {
        Self::canonical_with_mutations(
            self.resources
                .iter()
                .chain(&other.resources)
                .cloned()
                .collect(),
            self.mutations
                .iter()
                .chain(&other.mutations)
                .cloned()
                .collect(),
        )
    }

    /// Discharges a resource, as `with` does. Mutations are never discharged
    /// this way: they are a static property of the parameter, not a runtime
    /// provider.
    pub fn without(&self, resource: &CheckedResource) -> Self {
        Self::canonical_with_mutations(
            self.resources
                .iter()
                .filter(|candidate| *candidate != resource)
                .cloned()
                .collect(),
            self.mutations.clone(),
        )
    }

    pub fn is_subset_of(&self, other: &Self) -> bool {
        self.resources
            .iter()
            .all(|resource| other.resources.contains(resource))
            && self
                .mutations
                .iter()
                .all(|mutation| other.covers_mutation(mutation))
    }

    fn covers_mutation(&self, mutation: &CheckedMutation) -> bool {
        self.mutations.contains(&CheckedMutation::Whole) || self.mutations.contains(mutation)
    }
}

impl fmt::Display for CheckedResourceSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("{")?;
        let mut first = true;
        for mutation in &self.mutations {
            if !first {
                formatter.write_str(", ")?;
            }
            first = false;
            match mutation {
                CheckedMutation::Whole => formatter.write_str("mut")?,
                CheckedMutation::Element(index) => write!(formatter, "mut {index}")?,
            }
        }
        for resource in &self.resources {
            if !first {
                formatter.write_str(", ")?;
            }
            first = false;
            write!(formatter, "{}", resource.value_type)?;
        }
        formatter.write_str("}")
    }
}

/// Renders a resource set exactly like `Display for CheckedResourceSet`,
/// except that `mut <index>` is rendered as `mut <name>` when `parameter` is
/// a product with a name at that position — used wherever the parameter
/// type is in hand (an owning function's signature) to produce the same
/// spelling a user would write.
fn format_named_resource_set(resources: &CheckedResourceSet, parameter: &CheckedType) -> String {
    let mut entries = Vec::new();
    for mutation in &resources.mutations {
        entries.push(match mutation {
            CheckedMutation::Whole => "mut".to_string(),
            CheckedMutation::Element(index) => {
                let name = match parameter {
                    CheckedType::Product(product) => product
                        .elements
                        .get(*index)
                        .and_then(|element| element.name.as_deref()),
                    _ => None,
                };
                match name {
                    Some(name) => format!("mut {name}"),
                    None => format!("mut {index}"),
                }
            }
        });
    }
    for resource in &resources.resources {
        entries.push(resource.value_type.to_string());
    }
    format!("{{{}}}", entries.join(", "))
}

impl CheckedType {
    pub fn float(float: FloatType) -> Self {
        match float {
            FloatType::F32 => Self::F32,
            FloatType::F64 => Self::F64,
        }
    }

    pub fn float_type(&self) -> Option<FloatType> {
        match self {
            Self::F32 => Some(FloatType::F32),
            Self::F64 => Some(FloatType::F64),
            _ => None,
        }
    }

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

    pub fn is_fully_known(&self) -> bool {
        match self {
            Self::Inferred | Self::Error | Self::TypeConstructor { .. } => false,
            Self::CPointer { pointee } => pointee.is_fully_known(),
            Self::Ref(value) => {
                matches!(value.as_ref(), Self::ErasedProduct(element) if element.is_fully_known())
                    || value.is_fully_known()
            }
            Self::Buffer(value) => value.is_fully_known(),
            Self::ErasedProduct(_) => false,
            Self::Product(product) => product
                .elements
                .iter()
                .all(|element| element.value_type.is_fully_known()),
            Self::Sum(sum) => sum.alternatives.iter().all(CheckedType::is_fully_known),
            Self::Function(function) => {
                function.parameter.is_fully_known()
                    && function
                        .resources
                        .resources
                        .iter()
                        .all(|resource| resource.value_type.is_fully_known())
                    && function.result.is_fully_known()
            }
            Self::Distinct { representation, .. } => representation.is_fully_known(),
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
            | Self::F32
            | Self::F64
            | Self::String
            | Self::StringLiteralSet(_)
            | Self::CString
            | Self::CChar
            | Self::Opaque { .. }
            | Self::Parameter { .. } => true,
        }
    }

    pub fn is_sized(&self) -> bool {
        match self {
            // Unresolved/error types are accepted here to avoid cascading placement
            // diagnostics; full inference and constructor application are checked
            // independently.
            Self::Inferred | Self::Error | Self::TypeConstructor { .. } => true,
            Self::ErasedProduct(_) => false,
            Self::Parameter { sized, .. } => *sized,
            Self::Product(product) => product
                .elements
                .iter()
                .all(|element| element.value_type.is_sized()),
            Self::Sum(sum) => sum.alternatives.iter().all(CheckedType::is_sized),
            Self::Distinct { representation, .. } => representation.is_sized(),
            // These values all have a statically known handle or scalar representation.
            Self::Ref(_)
            | Self::Buffer(_)
            | Self::CPointer { .. }
            | Self::Function(_)
            | Self::I32
            | Self::I8
            | Self::I16
            | Self::I64
            | Self::U8
            | Self::U16
            | Self::U32
            | Self::U64
            | Self::ISize
            | Self::USize
            | Self::F32
            | Self::F64
            | Self::String
            | Self::StringLiteralSet(_)
            | Self::CString
            | Self::CChar
            | Self::Opaque { .. } => true,
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
            Self::F32 => formatter.write_str("F32"),
            Self::F64 => formatter.write_str("F64"),
            Self::String => formatter.write_str("String"),
            Self::StringLiteralSet(values) => {
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        formatter.write_str(" | ")?;
                    }
                    formatter.write_str(&crate::string_literal::encode(value))?;
                }
                Ok(())
            }
            Self::Ref(value) => match value.as_ref() {
                Self::ErasedProduct(element) => format_type_application(formatter, "Slice", element),
                _ => format_type_application(formatter, "Ref", value),
            },
            Self::Buffer(value) => format_type_application(formatter, "Buffer", value),
            Self::ErasedProduct(value) => write!(formatter, "{value}[]"),
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
                if function.resources.resources.is_empty() && function.resources.mutations.is_empty()
                {
                    write!(formatter, "{} -> {}", function.parameter, function.result)
                } else {
                    write!(
                        formatter,
                        "{} ->{} {}",
                        function.parameter,
                        format_named_resource_set(&function.resources, &function.parameter),
                        function.result
                    )
                }
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
    if matches!(argument, CheckedType::Sum(_) | CheckedType::Function(_))
        || matches!(argument, CheckedType::StringLiteralSet(values) if values.len() > 1)
    {
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
    expression_resources: HashMap<SyntaxId, CheckedResourceSet>,
    resource_types: HashMap<SyntaxId, CheckedResource>,
    function_bounds: HashMap<FunctionId, Vec<CheckedTraitBound>>,
    trait_method_types: HashMap<TraitMethodId, CheckedType>,
    trait_parameter_arguments: HashMap<TraitId, Vec<CheckedType>>,
    trait_functional_dependencies: HashMap<TraitId, Vec<CheckedFunctionalDependency>>,
    trait_dispatches: HashMap<SyntaxId, CheckedTraitDispatch>,
    trait_implementations: Vec<CheckedTraitImplementation>,
    expression_coercions: HashMap<SyntaxId, CheckedCoercion>,
    propagations: HashMap<SyntaxId, CheckedPropagation>,
    matches: HashMap<SyntaxId, CheckedMatch>,
    logicals: HashMap<SyntaxId, CheckedLogical>,
    accesses: HashMap<SyntaxId, CheckedAccess>,
    pattern_types: HashMap<SyntaxId, CheckedType>,
    string_representation: Option<CheckedType>,
    ownership: crate::ownership::OwnershipInfo,
    copy_trait: Option<TraitId>,
    drop_trait: Option<TraitId>,
    default_trait: Option<TraitId>,
    index_trait: Option<TraitId>,
    mutate_index_trait: Option<TraitId>,
    into_iterator_trait: Option<TraitId>,
    iterator_trait: Option<TraitId>,
    io_type: Option<TypeId>,
    mutated_parameter_symbols: HashSet<SymbolId>,
    method_symbols: HashMap<SyntaxId, SymbolId>,
}

impl TypedModule {
    pub fn resolved(&self) -> &ResolvedModule {
        &self.resolved
    }

    /// Whether `symbol` is addressable rather than a plain SSA value: an
    /// ordinary `mut` binding or a parameter covered by an explicit mutation
    /// effect. Parameter markers are effects, not resolver-level mutable
    /// bindings, so the checked set must supplement the resolver here.
    pub fn has_mutable_storage(&self, symbol: SymbolId) -> bool {
        self.resolved.has_mutable_storage(symbol) || self.mutated_parameter_symbols.contains(&symbol)
    }

    pub fn is_mutated_parameter(&self, symbol: SymbolId) -> bool {
        self.mutated_parameter_symbols.contains(&symbol)
    }

    pub fn syntax(&self) -> &Module {
        self.resolved.syntax()
    }

    pub fn functions(&self) -> &[ResolvedFunction] {
        self.resolved.functions()
    }

    pub fn symbol_for(&self, syntax_id: SyntaxId) -> Option<SymbolId> {
        self.method_symbols
            .get(&syntax_id)
            .copied()
            .or_else(|| self.resolved.symbol_for(syntax_id))
    }

    pub fn function_for(&self, syntax_id: SyntaxId) -> Option<FunctionId> {
        self.resolved.function_for(syntax_id)
    }

    pub fn type_of_expression(&self, syntax_id: SyntaxId) -> Option<&CheckedType> {
        self.expression_types.get(&syntax_id)
    }

    pub fn resources_of_expression(&self, syntax_id: SyntaxId) -> Option<&CheckedResourceSet> {
        self.expression_resources.get(&syntax_id)
    }

    pub fn resource_for_expression(&self, syntax_id: SyntaxId) -> Option<&CheckedResource> {
        self.resource_types.get(&syntax_id)
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

    pub fn logical_for(&self, syntax_id: SyntaxId) -> Option<&CheckedLogical> {
        self.logicals.get(&syntax_id)
    }

    pub fn access_for(&self, syntax_id: SyntaxId) -> Option<&CheckedAccess> {
        self.accesses.get(&syntax_id)
    }

    pub fn type_of_pattern(&self, syntax_id: SyntaxId) -> Option<&CheckedType> {
        self.pattern_types.get(&syntax_id)
    }

    pub(crate) fn string_representation(&self) -> Option<&CheckedType> {
        self.string_representation.as_ref()
    }

    pub fn is_copy_type(&self, value_type: &CheckedType) -> bool {
        is_copy_type(
            value_type,
            self.copy_trait,
            self.drop_trait,
            self.io_type,
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
            self.io_type,
            &self.trait_implementations,
            &bounds,
        )
    }

    pub(crate) fn is_io_type(&self, value_type: &CheckedType) -> bool {
        matches!(value_type, CheckedType::Opaque { id, .. } if Some(*id) == self.io_type)
    }

    pub(crate) fn io_resource(&self) -> Option<CheckedResource> {
        self.io_type.map(|id| CheckedResource {
            value_type: CheckedType::Opaque {
                id,
                name: "IO".to_owned(),
                arguments: Vec::new(),
            },
        })
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
                implementation.trait_id == drop_trait
                    && implementation.arguments.len() == 1
                    && &implementation.arguments[0] == value_type
            })
            .and_then(|implementation| implementation.methods.values().next().copied())
    }

    pub(crate) fn is_default_trait(&self, trait_id: TraitId) -> bool {
        self.default_trait == Some(trait_id)
    }

    pub(crate) fn structural_trait_method(
        &self,
        trait_id: TraitId,
        arguments: &[CheckedType],
    ) -> Option<StructuralTraitMethod> {
        structural_trait_arguments(
            trait_id,
            arguments,
            self.index_trait,
            self.mutate_index_trait,
            self.into_iterator_trait,
            self.iterator_trait,
            |value_type| self.is_copy_type(value_type),
        )
        .map(|(_, method)| method)
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
        arguments: &[CheckedType],
        method: TraitMethodId,
    ) -> Option<FunctionId> {
        let mut seen = Vec::new();
        let (index, _) = dispatch_matching_implementations(
            &self.trait_implementations,
            trait_id,
            arguments,
            &mut seen,
        )
        .into_iter()
        .next()?;
        self.trait_implementations[index]
            .methods
            .get(&method)
            .copied()
    }

    pub(crate) fn complete_trait_arguments(
        &self,
        trait_id: TraitId,
        arguments: &[CheckedType],
    ) -> Option<Vec<CheckedType>> {
        if let Some((arguments, _)) = structural_trait_arguments(
            trait_id,
            arguments,
            self.index_trait,
            self.mutate_index_trait,
            self.into_iterator_trait,
            self.iterator_trait,
            |value_type| self.is_copy_type(value_type),
        ) {
            return Some(arguments);
        }
        if !arguments.iter().any(contains_inferred_type) {
            return Some(arguments.to_vec());
        }
        let parameters = self.trait_parameter_arguments.get(&trait_id)?;
        let dependencies = self.trait_functional_dependencies.get(&trait_id)?;
        if dependencies.is_empty() {
            return None;
        }
        let query = trait_parameter_values(parameters, arguments);
        let mut matches = self
            .trait_implementations
            .iter()
            .filter(|implementation| implementation.trait_id == trait_id)
            .filter(|implementation| {
                let candidate = trait_parameter_values(parameters, &implementation.arguments);
                query.iter().all(|(parameter, value)| {
                    contains_inferred_type(value)
                        || candidate
                            .get(parameter)
                            .is_some_and(|candidate| candidate == value)
                })
            });
        let mut completed = matches.next()?.arguments.clone();
        for candidate in matches {
            completed = merge_trait_arguments(&completed, &candidate.arguments)?;
        }
        Some(completed)
    }

    pub(crate) fn instantiated_trait_method_type(
        &self,
        trait_id: TraitId,
        arguments: &[CheckedType],
        method: TraitMethodId,
    ) -> Option<CheckedFunctionType> {
        let parameters = self.trait_parameter_arguments.get(&trait_id)?;
        if parameters.len() != arguments.len() {
            return None;
        }
        let mut substitutions = HashMap::new();
        if !parameters
            .iter()
            .zip(arguments)
            .all(|(parameter, argument)| {
                infer_type_parameters(parameter, argument, &mut substitutions)
            })
        {
            return None;
        }
        let value_type = substitute_type(
            self.trait_method_types.get(&method)?.clone(),
            &substitutions,
        );
        let CheckedType::Function(function) = value_type else {
            return None;
        };
        Some(function)
    }
}

#[derive(Default)]
pub struct TypeChecker {
    expression_types: HashMap<SyntaxId, CheckedType>,
    symbol_types: HashMap<SymbolId, CheckedType>,
    function_types: HashMap<FunctionId, CheckedFunctionType>,
    expression_resources: HashMap<SyntaxId, CheckedResourceSet>,
    resource_types: HashMap<SyntaxId, CheckedResource>,
    function_bounds: HashMap<FunctionId, Vec<CheckedTraitBound>>,
    function_subtype_bounds: HashMap<FunctionId, Vec<CheckedSubtypeBound>>,
    trait_method_types: HashMap<TraitMethodId, CheckedType>,
    trait_parameter_arguments: HashMap<TraitId, Vec<CheckedType>>,
    trait_functional_dependencies: HashMap<TraitId, Vec<CheckedFunctionalDependency>>,
    trait_prerequisites: HashMap<TraitId, Vec<CheckedTraitBound>>,
    default_function_traits: HashMap<FunctionId, TraitId>,
    trait_dispatches: HashMap<SyntaxId, CheckedTraitDispatch>,
    trait_implementations: Vec<CheckedTraitImplementation>,
    impl_function_types: HashMap<FunctionId, CheckedFunctionType>,
    expression_coercions: HashMap<SyntaxId, CheckedCoercion>,
    propagations: HashMap<SyntaxId, CheckedPropagation>,
    matches: HashMap<SyntaxId, CheckedMatch>,
    logicals: HashMap<SyntaxId, CheckedLogical>,
    accesses: HashMap<SyntaxId, CheckedAccess>,
    pattern_types: HashMap<SyntaxId, CheckedType>,
    method_symbols: HashMap<SyntaxId, SymbolId>,
    symbol_companion_types: HashMap<SymbolId, TypeId>,
    function_result_companion_types: HashMap<FunctionId, TypeId>,
    string_representation: Option<CheckedType>,
    copy_trait: Option<TraitId>,
    sized_trait: Option<TraitId>,
    quote_result_trait: Option<TraitId>,
    drop_trait: Option<TraitId>,
    default_trait: Option<TraitId>,
    index_trait: Option<TraitId>,
    mutate_index_trait: Option<TraitId>,
    into_iterator_trait: Option<TraitId>,
    iterator_trait: Option<TraitId>,
    active_function_bounds: Vec<Vec<CheckedTraitBound>>,
    active_subtype_bounds: Vec<Vec<CheckedSubtypeBound>>,
    /// Type parameters declared by each generic `def` currently being
    /// checked, innermost last. A local `let` inside the body may mention
    /// any parameter in this stack without being "unconstrained" — it is
    /// bound by an enclosing generic scheme, not free.
    active_generic_parameters: Vec<HashSet<TypeParameterId>>,
    /// Guards recursive dispatch through conditional trait implementations:
    /// verifying a conditional impl's own bounds can re-enter obligation
    /// resolution, which can in turn re-examine the same implementation. A
    /// query already on this stack is treated as unsatisfied (fail closed)
    /// rather than accepted coinductively.
    checking_trait_obligations: RefCell<Vec<(TraitId, Vec<CheckedType>)>>,
    function_symbols: HashMap<SymbolId, FunctionId>,
    top_level_bindings: HashMap<SymbolId, Binding>,
    checking_bindings: HashSet<SymbolId>,
    checked_bindings: HashSet<SymbolId>,
    checking_functions: HashSet<FunctionId>,
    checking_modules: Vec<ModuleId>,
    checked_functions: HashSet<FunctionId>,
    type_declarations: HashMap<TypeId, TypeDeclaration>,
    resolved_named_types: HashMap<TypeId, CheckedType>,
    resolving_named_types: HashSet<TypeId>,
    return_contexts: Vec<CheckedType>,
    return_contributions: Vec<Vec<ReturnContribution>>,
    pending_propagations: Vec<Vec<(SyntaxId, CheckedType, usize, Span)>>,
    loop_contexts: Vec<LoopCheckContext>,
    did_return: bool,
    return_reachable: bool,
    diagnostics: Vec<Diagnostic>,
    io_type: Option<TypeId>,
    /// Parameter symbols covered by an explicit function effect, parameter
    /// marker, or trait contract. Only these are writable in function bodies.
    mutable_parameter_symbols: HashSet<SymbolId>,
    /// Positional parameter symbols for each function, indexed the same way
    /// as `CheckedMutation::Element` and the source `MutationTargetKind`:
    /// element-wise for a product pattern, otherwise a single "whole
    /// parameter" entry at index 0.
    parameter_symbols: HashMap<FunctionId, Vec<Option<SymbolId>>>,
    /// The same explicit set retained for code generation and capture layout.
    mutated_parameter_symbols: HashSet<SymbolId>,
}

impl TypeChecker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Type-checks on a dedicated thread with a generous stack size.
    /// Resource inference always runs, and its per-function AST
    /// walk adds recursion depth on top of `check_expression`'s own —
    /// deeply nested or macro-generated code (the standard library
    /// included) can exceed a default 2MB thread stack, which some test
    /// harnesses use for spawned test threads even though a typical `main`
    /// thread does not.
    pub fn check(self, module: ResolvedModule) -> Result<TypedModule, Vec<Diagnostic>> {
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(move || self.check_inner(module))
            .expect("type-checking thread should spawn")
            .join()
            .expect("type-checking should not panic")
    }

    fn check_inner(mut self, module: ResolvedModule) -> Result<TypedModule, Vec<Diagnostic>> {
        self.io_type = module
            .type_declarations()
            .keys()
            .find(|id| module.builtin_type(**id) == Some(BuiltinType::IO))
            .copied();
        self.copy_trait = module.standard_trait("Copy");
        self.sized_trait = module.standard_trait("Sized");
        self.quote_result_trait = module.standard_trait("ParseQuoteResult");
        self.drop_trait = module.standard_trait("Drop");
        self.default_trait = module.standard_trait("Default");
        self.index_trait = module.standard_trait("Index");
        self.mutate_index_trait = module.standard_trait("MutateIndex");
        self.into_iterator_trait = module.standard_trait("IntoIterator");
        self.iterator_trait = module.standard_trait("Iterator");
        self.collect_type_declarations(&module);
        self.collect_string_representation(&module);
        self.collect_traits(&module);
        self.validate_indexing_trait_method_types(&module);
        self.collect_trait_implementations(&module);
        self.validate_trait_implementation_prerequisites(&module);
        self.seed_constructors(&module);
        self.seed_singleton_values(&module);
        self.collect_top_level_bindings(&module);
        self.seed_declared_bindings(&module);
        self.validate_intrinsics(&module);
        self.seed_function_types(&module);

        let module_order = module.program().initialization_order().to_vec();
        for module_id in module_order {
            // Each module's top-level statements execute as their own
            // initializer (a dedicated function in codegen), so reachability
            // tracking must be live here exactly as it is inside a function
            // body — otherwise a top-level `break`/`continue` never records
            // itself (see the `return_reachable` guard on `Item::Break`/
            // `Continue`), and `check_loop_expression` then treats a
            // perfectly ordinary `loop { ...; break }` as never exiting,
            // marking every subsequent top-level item unreachable and
            // silently skipping their checks.
            let outer_did_return = self.did_return;
            let outer_return_reachable = self.return_reachable;
            self.did_return = false;
            self.return_reachable = true;
            self.checking_modules.push(module_id);
            for item in &module.program().module(module_id).syntax.items {
                self.check_item(&module, item);
            }
            self.checking_modules.pop();
            self.did_return = outer_did_return;
            self.return_reachable = outer_return_reachable;
        }
        let function_ids = module
            .functions()
            .iter()
            .map(|function| function.id)
            .collect::<Vec<_>>();
        for function_id in function_ids {
            self.ensure_function_checked(&module, function_id);
        }
        self.infer_resources(&module);

        if !self.diagnostics.is_empty() {
            return Err(self.diagnostics);
        }

        let typed = TypedModule {
            resolved: module,
            expression_types: self.expression_types,
            symbol_types: self.symbol_types,
            function_types: self.function_types,
            expression_resources: self.expression_resources,
            resource_types: self.resource_types,
            function_bounds: self.function_bounds,
            trait_method_types: self.trait_method_types,
            trait_parameter_arguments: self.trait_parameter_arguments,
            trait_functional_dependencies: self.trait_functional_dependencies,
            trait_dispatches: self.trait_dispatches,
            trait_implementations: self.trait_implementations,
            expression_coercions: self.expression_coercions,
            propagations: self.propagations,
            matches: self.matches,
            logicals: self.logicals,
            accesses: self.accesses,
            pattern_types: self.pattern_types,
            string_representation: self.string_representation,
            ownership: crate::ownership::OwnershipInfo::default(),
            copy_trait: self.copy_trait,
            drop_trait: self.drop_trait,
            default_trait: self.default_trait,
            index_trait: self.index_trait,
            mutate_index_trait: self.mutate_index_trait,
            into_iterator_trait: self.into_iterator_trait,
            iterator_trait: self.iterator_trait,
            io_type: self.io_type,
            mutated_parameter_symbols: self.mutated_parameter_symbols,
            method_symbols: self.method_symbols,
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

    fn collect_string_representation(&mut self, module: &ResolvedModule) {
        let Some((_, declaration)) = self
            .type_declarations
            .iter()
            .find(|(id, _)| module.builtin_type(**id) == Some(BuiltinType::String))
        else {
            return;
        };
        let Some(underlying) = declaration.underlying.clone() else {
            return;
        };
        let span = declaration.syntax.span.clone();
        let representation = self.resolve_source_type(module, &underlying);
        let expected = expected_string_representation();
        if representation == expected {
            self.string_representation = Some(representation);
        } else if representation != CheckedType::Error {
            self.diagnostics.push(Diagnostic::new(
                span,
                format!(
                    "standard library type `String` must be represented by `Slice U8`, found `{representation}`"
                ),
            ));
        }
    }

    fn collect_traits(&mut self, module: &ResolvedModule) {
        match self.sized_trait.and_then(|id| module.traits().get(&id)) {
            Some(sized)
                if sized.declaration.visibility == crate::Visibility::Public
                    && sized.declaration.type_parameters.len() == 1
                    && sized.parameters.len() == 1
                    && sized.declaration.members.is_empty() => {}
            _ => self.diagnostics.push(Diagnostic::new(
                Span::Compiler,
                "standard library must declare public empty trait `Sized`",
            )),
        }
        match self.copy_trait.and_then(|id| module.traits().get(&id)) {
            Some(copy)
                if copy.declaration.visibility == crate::Visibility::Public
                    && copy.declaration.type_parameters.len() == 1
                    && copy.parameters.len() == 1
                    && copy.declaration.members.is_empty() => {}
            _ => self.diagnostics.push(Diagnostic::new(
                Span::Compiler,
                "standard library must declare public empty trait `Copy`",
            )),
        }
        match self
            .quote_result_trait
            .and_then(|id| module.traits().get(&id))
        {
            Some(quote_result)
                if quote_result.declaration.visibility == crate::Visibility::Public
                    && quote_result.declaration.type_parameters.len() == 1
                    && quote_result.parameters.len() == 1
                    && quote_result.declaration.members.is_empty() => {}
            _ => self.diagnostics.push(Diagnostic::new(
                Span::Compiler,
                "standard library must declare public empty trait `ParseQuoteResult`",
            )),
        }
        match self.drop_trait.and_then(|id| module.traits().get(&id)) {
            Some(drop)
                if drop.declaration.visibility == crate::Visibility::Public
                    && drop.declaration.type_parameters.len() == 1
                    && drop.parameters.len() == 1
                    && drop.declaration.members.len() == 1
                    && drop.declaration.members[0].name == "drop" => {}
            _ => self.diagnostics.push(Diagnostic::new(
                Span::Compiler,
                "standard library must declare public trait `Drop` with one `drop` member",
            )),
        }
        match self.default_trait.and_then(|id| module.traits().get(&id)) {
            Some(default)
                if default.declaration.visibility == crate::Visibility::Public
                    && default.declaration.type_parameters.len() == 1
                    && default.parameters.len() == 1
                    && default.declaration.members.len() == 1
                    && default.declaration.members[0].name == "default" => {}
            _ => self.diagnostics.push(Diagnostic::new(
                Span::Compiler,
                "standard library must declare public trait `Default` with one `default` member",
            )),
        }
        for (trait_id, trait_name, member_name) in [
            (self.index_trait, "Index", "index"),
            (self.mutate_index_trait, "MutateIndex", "mutate_index"),
        ] {
            let valid = trait_id
                .and_then(|id| module.traits().get(&id))
                .is_some_and(|resolved| {
                    resolved.declaration.visibility == crate::Visibility::Public
                        && resolved.parameters.len() == 3
                        && resolved.declaration.members.len() == 1
                        && resolved.declaration.members[0].name == member_name
                        && resolved.functional_dependencies.len() == 1
                        && resolved.functional_dependencies[0].determinants
                            == resolved.parameters[..2]
                        && resolved.functional_dependencies[0].dependent == resolved.parameters[2]
                });
            if !valid {
                self.diagnostics.push(Diagnostic::new(
                    Span::Compiler,
                    format!(
                        "standard library must declare public trait `{trait_name}` with three parameters, the required functional dependency, and one `{member_name}` member"
                    ),
                ));
            }
        }
        for (trait_id, trait_name, member_name) in [
            (self.iterator_trait, "Iterator", "next"),
            (self.into_iterator_trait, "IntoIterator", "into_iterator"),
        ] {
            let valid = trait_id
                .and_then(|id| module.traits().get(&id))
                .is_some_and(|resolved| {
                    resolved.declaration.visibility == crate::Visibility::Public
                        && resolved.parameters.len() == 2
                        && resolved.declaration.members.len() == 1
                        && resolved.declaration.members[0].name == member_name
                        && resolved.functional_dependencies.len() == 1
                        && resolved.functional_dependencies[0].determinants
                            == resolved.parameters[..1]
                        && resolved.functional_dependencies[0].dependent == resolved.parameters[1]
                });
            if !valid {
                self.diagnostics.push(Diagnostic::new(
                    Span::Compiler,
                    format!(
                        "standard library must declare public trait `{trait_name}` with two parameters, the required functional dependency, and one `{member_name}` member"
                    ),
                ));
            }
        }
        for resolved_trait in module.traits().values() {
            let parameter_arguments = resolved_trait
                .declaration
                .type_parameters
                .iter()
                .map(|parameter| self.checked_type_parameter_pattern(module, parameter))
                .collect::<Vec<_>>();
            self.trait_parameter_arguments
                .insert(resolved_trait.id, parameter_arguments);
            self.trait_functional_dependencies.insert(
                resolved_trait.id,
                resolved_trait
                    .functional_dependencies
                    .iter()
                    .map(|dependency| CheckedFunctionalDependency {
                        determinants: dependency.determinants.clone(),
                        dependent: dependency.dependent,
                    })
                    .collect(),
            );
            let mut prerequisites = Vec::new();
            for prerequisite in &resolved_trait.declaration.prerequisites {
                if let Some(prerequisite) = self.resolve_trait_bound(module, prerequisite) {
                    prerequisites.push(prerequisite);
                }
            }
            self.trait_prerequisites
                .insert(resolved_trait.id, prerequisites);
        }
        for resolved_trait in module.traits().values() {
            let prerequisites = self.expand_trait_bounds(
                self.trait_prerequisites
                    .get(&resolved_trait.id)
                    .cloned()
                    .unwrap_or_default(),
            );
            if self.bounds_violate_functional_dependencies(&prerequisites) {
                self.diagnostics.push(Diagnostic::new(
                    resolved_trait.declaration.syntax.span.clone(),
                    "trait prerequisites conflict with a functional dependency",
                ));
            }
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
                let parameter_names = resolved_trait
                    .declaration
                    .type_parameters
                    .iter()
                    .flat_map(TypeParameterPattern::names)
                    .collect::<Vec<_>>();
                for (parameter, name) in resolved_trait.parameters.iter().zip(parameter_names) {
                    if !contains_type_parameter_id(&value_type, *parameter) {
                        self.diagnostics.push(Diagnostic::new(
                            member.syntax.span.clone(),
                            format!(
                                "trait member `{}` must mention trait parameter `{name}`",
                                member.name
                            ),
                        ));
                    }
                }
                if contains_inferred_type(&value_type) {
                    self.diagnostics.push(Diagnostic::new(
                        member.syntax.span.clone(),
                        "trait member types cannot contain `_`",
                    ));
                }
                if let Some(function) = resolved_trait.default_methods.get(method).copied()
                    && let CheckedType::Function(function_type) = value_type.clone()
                {
                    self.impl_function_types.insert(function, function_type);
                    self.default_function_traits
                        .insert(function, resolved_trait.id);
                }
                self.trait_method_types.insert(*method, value_type);
            }
        }
    }

    fn collect_trait_implementations(&mut self, module: &ResolvedModule) {
        for implementation in module.trait_implementations() {
            let span = implementation
                .arguments
                .first()
                .map(|argument| argument.syntax().span.clone())
                .unwrap_or(Span::Compiler);
            if Some(implementation.trait_id) == self.sized_trait {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    "`Sized` is implemented structurally and cannot be implemented explicitly",
                ));
                continue;
            }
            if Some(implementation.trait_id) == self.copy_trait {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    "`Copy` is implemented structurally and cannot be implemented explicitly",
                ));
                continue;
            }
            if Some(implementation.trait_id) == self.quote_result_trait {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    "`ParseQuoteResult` is compiler-defined and cannot be implemented explicitly",
                ));
                continue;
            }
            let Some((arguments, substitutions)) = self.resolve_trait_arguments(
                module,
                implementation.trait_id,
                &implementation.arguments,
                span.clone(),
                false,
            ) else {
                continue;
            };
            let target = &arguments[0];
            if Some(implementation.trait_id) == self.default_trait
                && matches!(target, CheckedType::Product(_))
            {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    "`Default` is derived structurally for products and cannot be implemented explicitly",
                ));
                continue;
            }
            if structural_trait_arguments(
                implementation.trait_id,
                &arguments,
                self.index_trait,
                self.mutate_index_trait,
                self.into_iterator_trait,
                self.iterator_trait,
                |value_type| {
                    is_copy_type(
                        value_type,
                        self.copy_trait,
                        self.drop_trait,
                        self.io_type,
                        &self.trait_implementations,
                        &[],
                    )
                },
            )
            .is_some()
            {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    "indexing and iteration traits are derived structurally for this product type and cannot be implemented explicitly",
                ));
                continue;
            }
            let declared_parameters: HashSet<TypeParameterId> =
                implementation.parameters.iter().copied().collect();
            if arguments.iter().any(|argument| {
                contains_inferred_type(argument)
                    || !argument.is_fully_known()
                    || type_parameter_ids(argument)
                        .iter()
                        .any(|id| !declared_parameters.contains(id))
            }) {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    if arguments.len() == 1 {
                        "trait implementation target must be fully concrete"
                    } else {
                        "trait implementation arguments must be fully concrete"
                    },
                ));
                continue;
            }
            if Some(implementation.trait_id) == self.drop_trait
                && !matches!(target, CheckedType::Distinct { .. })
            {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    "`Drop` may only be implemented for a represented nominal type",
                ));
                continue;
            }
            let canonical_arguments = canonicalize_impl_header(&declared_parameters, &arguments);
            if self.trait_implementations.iter().any(|existing| {
                existing.trait_id == implementation.trait_id
                    && canonicalize_impl_header(&existing.parameters, &existing.arguments)
                        == canonical_arguments
            }) {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    "duplicate trait implementation for these arguments",
                ));
                continue;
            }
            let dependencies = self
                .trait_functional_dependencies
                .get(&implementation.trait_id)
                .cloned()
                .unwrap_or_default();
            let parameters = &self.trait_parameter_arguments[&implementation.trait_id];
            let new_values = trait_parameter_values(parameters, &canonical_arguments);
            let conflict = self.trait_implementations.iter().any(|existing| {
                if existing.trait_id != implementation.trait_id {
                    return false;
                }
                let existing_canonical =
                    canonicalize_impl_header(&existing.parameters, &existing.arguments);
                let existing_values = trait_parameter_values(parameters, &existing_canonical);
                dependencies.iter().any(|dependency| {
                    dependency.determinants.iter().all(|determinant| {
                        new_values.get(determinant) == existing_values.get(determinant)
                    }) && new_values.get(&dependency.dependent)
                        != existing_values.get(&dependency.dependent)
                })
            });
            if conflict {
                self.diagnostics.push(Diagnostic::new(
                    span.clone(),
                    "conflicting trait implementations violate a functional dependency",
                ));
                continue;
            }
            let resolved_trait = &module.traits()[&implementation.trait_id];
            let mut methods = implementation.methods.clone();
            for method in &resolved_trait.methods {
                let member = module.trait_method(*method).expect("resolved trait member");
                let Some(function_id) = implementation.methods.get(method).copied() else {
                    if let Some(default) = resolved_trait.default_methods.get(method).copied() {
                        methods.insert(*method, default);
                        continue;
                    }
                    self.diagnostics.push(Diagnostic::new(
                        span.clone(),
                        format!("implementation is missing member `{}`", member.name),
                    ));
                    continue;
                };
                let expected =
                    substitute_type(self.trait_method_types[method].clone(), &substitutions);
                if let CheckedType::Function(function_type) = expected {
                    self.impl_function_types.insert(function_id, function_type);
                }
            }
            let bounds = implementation
                .trait_bounds
                .iter()
                .filter_map(|bound| self.resolve_trait_bound(module, bound))
                .collect();
            self.trait_implementations.push(CheckedTraitImplementation {
                span,
                trait_id: implementation.trait_id,
                parameters: declared_parameters,
                arguments,
                bounds,
                methods,
            });
        }
    }

    fn validate_indexing_trait_method_types(&mut self, module: &ResolvedModule) {
        for (trait_id, trait_name, arity, result_parameter, mutations) in [
            (self.index_trait, "Index", 2, Some(2usize), Vec::new()),
            (
                self.mutate_index_trait,
                "MutateIndex",
                3,
                None,
                vec![CheckedMutation::Element(0)],
            ),
        ] {
            let Some(trait_id) = trait_id else {
                continue;
            };
            let Some(resolved) = module.traits().get(&trait_id) else {
                continue;
            };
            let Some(method) = resolved.methods.first() else {
                continue;
            };
            let Some(parameters) = self.trait_parameter_arguments.get(&trait_id) else {
                continue;
            };
            if parameters.len() != 3 {
                continue;
            }
            let expected = CheckedType::Function(CheckedFunctionType {
                parameter: Box::new(CheckedType::Product(CheckedProductType {
                    elements: parameters[..arity]
                        .iter()
                        .cloned()
                        .map(|value_type| CheckedTypeElement {
                            name: None,
                            value_type,
                        })
                        .collect(),
                    variadic: false,
                })),
                resources: CheckedResourceSet::canonical_with_mutations(Vec::new(), mutations),
                result: Box::new(
                    result_parameter.map_or_else(CheckedType::empty_product, |index| {
                        parameters[index].clone()
                    }),
                ),
            });
            if self.trait_method_types.get(method) != Some(&expected) {
                self.diagnostics.push(Diagnostic::new(
                    Span::Compiler,
                    format!("standard-library trait `{trait_name}` has an invalid member type"),
                ));
            }
        }
    }

    fn validate_trait_implementation_prerequisites(&mut self, module: &ResolvedModule) {
        let mut diagnostics = Vec::new();
        for implementation in &self.trait_implementations {
            let bounds = self.expand_trait_bounds(vec![CheckedTraitBound {
                trait_id: implementation.trait_id,
                arguments: implementation.arguments.clone(),
            }]);
            for prerequisite in bounds.into_iter().skip(1) {
                if self.trait_obligation_available(prerequisite.trait_id, &prerequisite.arguments) {
                    continue;
                }
                let trait_name = &module.traits()[&prerequisite.trait_id].declaration.name;
                let arguments = prerequisite
                    .arguments
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                diagnostics.push(Diagnostic::new(
                    implementation.span.clone(),
                    format!("trait prerequisite `{trait_name} {arguments}` is not satisfied"),
                ));
            }
        }
        self.diagnostics.extend(diagnostics);
    }

    fn resolve_trait_arguments(
        &mut self,
        module: &ResolvedModule,
        trait_id: TraitId,
        source_arguments: &[Type],
        span: Span,
        allow_inference: bool,
    ) -> Option<(Vec<CheckedType>, HashMap<TypeParameterId, CheckedType>)> {
        let resolved_trait = &module.traits()[&trait_id];
        // Before traits had arity, a unary trait target such as `Box I32` was
        // stored as one applied type. Preserve that source form by rejoining
        // the application spine when the resolved trait is unary.
        let normalized_arguments;
        let source_arguments = if resolved_trait.declaration.type_parameters.len() == 1
            && source_arguments.len() > 1
        {
            let mut application = source_arguments[0].clone();
            for argument in &source_arguments[1..] {
                application = Type::Application(crate::TypeApplication {
                    syntax: application.syntax().clone(),
                    callee: Box::new(application),
                    argument: Box::new(argument.clone()),
                });
            }
            normalized_arguments = vec![application];
            normalized_arguments.as_slice()
        } else {
            source_arguments
        };
        let expected_arity = resolved_trait.declaration.type_parameters.len();
        if source_arguments.len() > expected_arity {
            self.diagnostics.push(Diagnostic::new(
                span,
                format!(
                    "trait `{}` expects {} compile-time argument{}, found {}",
                    resolved_trait.declaration.name,
                    expected_arity,
                    if expected_arity == 1 { "" } else { "s" },
                    source_arguments.len()
                ),
            ));
            return None;
        }
        let mut trait_defaults: HashMap<TypeParameterId, CheckedType> = HashMap::new();
        for bound in &resolved_trait.declaration.default_bounds {
            if let Some(checked) = self.resolve_default_bound(module, bound) {
                trait_defaults.insert(checked.parameter, checked.default);
            }
        }
        let has_default = |pattern: &TypeParameterPattern| -> bool {
            match pattern {
                TypeParameterPattern::Binding(binding) => module
                    .type_parameter_for(binding.syntax.id)
                    .is_some_and(|id| trait_defaults.contains_key(&id)),
                _ => false,
            }
        };
        let arity_satisfied = source_arguments.len() >= expected_arity
            || resolved_trait.declaration.type_parameters[source_arguments.len()..]
                .iter()
                .all(has_default);
        if !allow_inference && !arity_satisfied {
            self.diagnostics.push(Diagnostic::new(
                span,
                format!(
                    "trait `{}` expects {} compile-time argument{}, found {}",
                    resolved_trait.declaration.name,
                    expected_arity,
                    if expected_arity == 1 { "" } else { "s" },
                    source_arguments.len()
                ),
            ));
            return None;
        }
        let mut default_substitutions: HashMap<TypeParameterId, CheckedType> = HashMap::new();
        let mut arguments = Vec::with_capacity(expected_arity);
        for (index, pattern) in resolved_trait.declaration.type_parameters.iter().enumerate() {
            let value = if index < source_arguments.len() {
                self.resolve_source_type(module, &source_arguments[index])
            } else if let TypeParameterPattern::Binding(binding) = pattern
                && let Some(param_id) = module.type_parameter_for(binding.syntax.id)
                && let Some(default) = trait_defaults.get(&param_id)
            {
                substitute_type(default.clone(), &default_substitutions)
            } else {
                CheckedType::Inferred
            };
            if let TypeParameterPattern::Binding(binding) = pattern
                && let Some(param_id) = module.type_parameter_for(binding.syntax.id)
            {
                default_substitutions.insert(param_id, value.clone());
            }
            arguments.push(value);
        }
        let mut substitutions = HashMap::new();
        // `Sized T` must be expressible while `T` is unsized: the obligation
        // itself is what proves that the argument satisfies `Sized`.
        if Some(trait_id) == self.sized_trait {
            return Some((arguments, substitutions));
        }
        for (pattern, argument) in resolved_trait
            .declaration
            .type_parameters
            .iter()
            .zip(&arguments)
        {
            if !self.bind_type_argument(module, pattern, argument, &mut substitutions) {
                return None;
            }
        }
        if allow_inference {
            let mut known = substitutions
                .iter()
                .filter_map(|(parameter, argument)| {
                    (!contains_inferred_type(argument)).then_some(*parameter)
                })
                .collect::<HashSet<_>>();
            loop {
                let before = known.len();
                for dependency in &resolved_trait.functional_dependencies {
                    if dependency
                        .determinants
                        .iter()
                        .all(|determinant| known.contains(determinant))
                    {
                        known.insert(dependency.dependent);
                    }
                }
                if known.len() == before {
                    break;
                }
            }
            let uninferable = substitutions.iter().find_map(|(parameter, argument)| {
                (contains_inferred_type(argument) && !known.contains(parameter))
                    .then_some(*parameter)
            });
            if let Some(parameter) = uninferable {
                let name = resolved_trait
                    .parameters
                    .iter()
                    .position(|candidate| *candidate == parameter)
                    .and_then(|index| {
                        resolved_trait
                            .declaration
                            .type_parameters
                            .iter()
                            .flat_map(TypeParameterPattern::names)
                            .nth(index)
                    })
                    .unwrap_or("_");
                self.diagnostics.push(Diagnostic::new(
                    span,
                    format!(
                        "trait argument for `{name}` cannot be inferred from functional dependencies"
                    ),
                ));
                return None;
            }
        }
        Some((arguments, substitutions))
    }

    fn resolve_trait_bound(
        &mut self,
        module: &ResolvedModule,
        bound: &crate::TraitBound,
    ) -> Option<CheckedTraitBound> {
        let trait_id = module.trait_for(bound.syntax.id)?;
        let (arguments, _) = self.resolve_trait_arguments(
            module,
            trait_id,
            &bound.arguments,
            bound.syntax.span.clone(),
            true,
        )?;
        Some(CheckedTraitBound {
            trait_id,
            arguments,
        })
    }

    fn resolve_subtype_bound(
        &mut self,
        module: &ResolvedModule,
        bound: &crate::SubtypeBound,
    ) -> Option<CheckedSubtypeBound> {
        let parameter = module.type_parameter_for(bound.syntax.id)?;
        let supertype = self.resolve_source_type(module, &bound.supertype);
        Some(CheckedSubtypeBound {
            parameter,
            supertype,
        })
    }

    fn resolve_default_bound(
        &mut self,
        module: &ResolvedModule,
        bound: &crate::DefaultTypeBound,
    ) -> Option<CheckedDefaultBound> {
        let parameter = module.type_parameter_for(bound.syntax.id)?;
        let default = self.resolve_source_type(module, &bound.default);
        Some(CheckedDefaultBound { parameter, default })
    }

    fn expand_trait_bounds(&self, bounds: Vec<CheckedTraitBound>) -> Vec<CheckedTraitBound> {
        let mut expanded = Vec::new();
        for bound in bounds {
            self.expand_trait_bound(bound, &mut expanded);
        }
        expanded
    }

    fn expand_trait_bound(&self, bound: CheckedTraitBound, expanded: &mut Vec<CheckedTraitBound>) {
        if expanded.contains(&bound) {
            return;
        }
        expanded.push(bound.clone());
        let Some(parameters) = self.trait_parameter_arguments.get(&bound.trait_id) else {
            return;
        };
        if parameters.len() != bound.arguments.len() {
            return;
        }
        let substitutions = trait_parameter_values(parameters, &bound.arguments);
        for prerequisite in self
            .trait_prerequisites
            .get(&bound.trait_id)
            .into_iter()
            .flatten()
        {
            let prerequisite = CheckedTraitBound {
                trait_id: prerequisite.trait_id,
                arguments: prerequisite
                    .arguments
                    .iter()
                    .cloned()
                    .map(|argument| substitute_type(argument, &substitutions))
                    .collect(),
            };
            self.expand_trait_bound(prerequisite, expanded);
        }
    }

    fn bounds_violate_functional_dependencies(&self, bounds: &[CheckedTraitBound]) -> bool {
        for (index, left) in bounds.iter().enumerate() {
            let Some(parameters) = self.trait_parameter_arguments.get(&left.trait_id) else {
                continue;
            };
            let left_values = trait_parameter_values(parameters, &left.arguments);
            for right in &bounds[index + 1..] {
                if right.trait_id != left.trait_id {
                    continue;
                }
                let right_values = trait_parameter_values(parameters, &right.arguments);
                if self
                    .trait_functional_dependencies
                    .get(&left.trait_id)
                    .into_iter()
                    .flatten()
                    .any(|dependency| {
                        dependency.determinants.iter().all(|determinant| {
                            let left = left_values.get(determinant);
                            let right = right_values.get(determinant);
                            left == right
                                && left.is_some_and(|value| !contains_inferred_type(value))
                        }) && left_values
                            .get(&dependency.dependent)
                            .zip(right_values.get(&dependency.dependent))
                            .is_some_and(|(left, right)| {
                                !contains_inferred_type(left)
                                    && !contains_inferred_type(right)
                                    && left != right
                            })
                    })
                {
                    return true;
                }
            }
        }
        false
    }

    fn seed_constructors(&mut self, module: &ResolvedModule) {
        for (symbol, id) in module.constructors() {
            if module.recursive_construction(*id) == Some(crate::RecursiveConstruction::Syntax) {
                continue;
            }
            let declaration = self.type_declarations[id].clone();
            let Some(underlying) = declaration.underlying.as_ref() else {
                continue;
            };
            // The constructor's arguments are the declaration's own type
            // parameters (see `checked_type_parameter_pattern` below), so
            // this instantiation is self-referential: it asks whether each
            // parameter satisfies the declaration's own bounds. Those bounds
            // must already be active for that reflexive check to succeed,
            // mirroring the push `instantiate_type_declaration` performs
            // around resolving the representation itself.
            let mut declaration_bounds = Vec::new();
            for bound in &declaration.trait_bounds {
                if let Some(bound) = self.resolve_trait_bound(module, bound) {
                    declaration_bounds.push(bound);
                }
            }
            let declaration_bounds = self.expand_trait_bounds(declaration_bounds);
            let mut declaration_subtype_bounds = Vec::new();
            for bound in &declaration.subtype_bounds {
                if let Some(bound) = self.resolve_subtype_bound(module, bound) {
                    declaration_subtype_bounds.push(bound);
                }
            }
            self.active_function_bounds.push(declaration_bounds);
            self.active_subtype_bounds.push(declaration_subtype_bounds);
            let parameter = self.resolve_source_type(module, underlying);
            let arguments = declaration
                .type_parameters
                .iter()
                .map(|pattern| self.checked_type_parameter_pattern(module, pattern))
                .collect::<Vec<_>>();
            let result = self.instantiate_type_declaration(module, *id, arguments);
            self.active_function_bounds.pop();
            self.active_subtype_bounds.pop();
            self.symbol_types.insert(
                *symbol,
                CheckedType::Function(CheckedFunctionType {
                    parameter: Box::new(parameter),
                    resources: CheckedResourceSet::default(),
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

    fn declared_type_parameters(
        &self,
        module: &ResolvedModule,
        type_parameters: &[TypeParameterPattern],
    ) -> HashSet<TypeParameterId> {
        type_parameters
            .iter()
            .flat_map(|pattern| {
                type_parameter_ids(&self.checked_type_parameter_pattern(module, pattern))
            })
            .collect()
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
                sized: binding.sized,
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
            TypeParameterPattern::Splice(_) => {
                unreachable!("type-parameter splices must be expanded before type checking")
            }
        }
    }

    fn collect_top_level_bindings(&mut self, module: &ResolvedModule) {
        for source_module in module.program().modules() {
            for item in &source_module.syntax.items {
                if let Item::Binding(binding) = item
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
                    Item::Binding(binding) => self.seed_binding_annotation(module, binding),
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
                    | Item::PatternBinding(_)
                    | Item::Assignment(_)
                    | Item::Return(_)
                    | Item::Break(_)
                    | Item::Continue(_)
                    | Item::Expression(_) => {}
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
            if let Some(id) = source_type_id(module, annotation) {
                self.symbol_companion_types.insert(symbol, id);
            }
        }
    }

    fn validate_intrinsics(&mut self, module: &ResolvedModule) {
        for (symbol, intrinsic) in module.intrinsic_functions() {
            let expected = match intrinsic {
                crate::IntrinsicFunction::ToString { value } => {
                    let parameter = match value {
                        crate::NumericType::Integer(integer) => CheckedType::integer(*integer),
                        crate::NumericType::Float(float) => CheckedType::float(*float),
                    };
                    CheckedType::Function(CheckedFunctionType {
                        parameter: Box::new(parameter),
                        resources: CheckedResourceSet::default(),
                        result: Box::new(CheckedType::String),
                    })
                }
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
                        resources: CheckedResourceSet::default(),
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
                        resources: CheckedResourceSet::default(),
                        result: Box::new(result),
                    })
                }
                crate::IntrinsicFunction::FloatBinary { float, .. } => {
                    let float = CheckedType::float(*float);
                    CheckedType::Function(CheckedFunctionType {
                        parameter: Box::new(CheckedType::Product(CheckedProductType {
                            elements: vec![
                                CheckedTypeElement { name: None, value_type: float.clone() },
                                CheckedTypeElement { name: None, value_type: float.clone() },
                            ],
                            variadic: false,
                        })),
                        resources: CheckedResourceSet::default(),
                        result: Box::new(float),
                    })
                }
                crate::IntrinsicFunction::FloatCompare { float, .. } => {
                    let float = CheckedType::float(*float);
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
                                CheckedTypeElement { name: None, value_type: float.clone() },
                                CheckedTypeElement { name: None, value_type: float },
                            ],
                            variadic: false,
                        })),
                        resources: CheckedResourceSet::default(),
                        result: Box::new(result),
                    })
                }
                crate::IntrinsicFunction::StringFromCString => {
                    CheckedType::Function(CheckedFunctionType {
                        parameter: Box::new(CheckedType::CString),
                        resources: CheckedResourceSet::default(),
                        result: Box::new(CheckedType::String),
                    })
                }
                crate::IntrinsicFunction::StringToCString => {
                    CheckedType::Function(CheckedFunctionType {
                        parameter: Box::new(CheckedType::String),
                        resources: CheckedResourceSet::default(),
                        result: Box::new(CheckedType::CString),
                    })
                }
                crate::IntrinsicFunction::StringAdd => CheckedType::Function(CheckedFunctionType {
                    parameter: Box::new(CheckedType::Product(CheckedProductType {
                        elements: vec![
                            CheckedTypeElement { name: None, value_type: CheckedType::String },
                            CheckedTypeElement { name: None, value_type: CheckedType::String },
                        ],
                        variadic: false,
                    })),
                    resources: CheckedResourceSet::default(),
                    result: Box::new(CheckedType::String),
                }),
                crate::IntrinsicFunction::SliceLength => self
                    .symbol_types
                    .get(symbol)
                    .cloned()
                    .filter(|value_type| matches!(
                        value_type,
                        CheckedType::Function(function)
                            if matches!(function.parameter.as_ref(),
                                CheckedType::Ref(payload)
                                    if matches!(payload.as_ref(), CheckedType::ErasedProduct(_)))
                                && *function.result == CheckedType::USize
                    ))
                    .unwrap_or(CheckedType::Error),
                crate::IntrinsicFunction::SliceFromRef => self
                    .symbol_types
                    .get(symbol)
                    .cloned()
                    .filter(|value_type| matches!(
                        value_type,
                        CheckedType::Function(function)
                            if matches!(function.parameter.as_ref(), CheckedType::Ref(_))
                                && matches!(function.result.as_ref(),
                                    CheckedType::Ref(payload)
                                        if matches!(payload.as_ref(), CheckedType::ErasedProduct(_)))
                    ))
                    .unwrap_or(CheckedType::Error),
                crate::IntrinsicFunction::BufferWithCapacity
                | crate::IntrinsicFunction::BufferLength
                | crate::IntrinsicFunction::BufferCapacity
                | crate::IntrinsicFunction::BufferPush
                | crate::IntrinsicFunction::BufferPop
                | crate::IntrinsicFunction::BufferGet
                | crate::IntrinsicFunction::BufferFreeze
                | crate::IntrinsicFunction::BufferTransfer => self
                    .symbol_types
                    .get(symbol)
                    .cloned()
                    .filter(|value_type| valid_buffer_intrinsic_type(value_type, *intrinsic))
                    .unwrap_or(CheckedType::Error),
                crate::IntrinsicFunction::RefReplace => self
                    .symbol_types
                    .get(symbol)
                    .cloned()
                    .filter(|value_type| match value_type {
                        CheckedType::Function(function) => {
                            let CheckedType::Product(product) = function.parameter.as_ref() else {
                                return false;
                            };
                            let [reference, replacement] = product.elements.as_slice() else {
                                return false;
                            };
                            matches!(&reference.value_type, CheckedType::Ref(payload)
                                if payload.as_ref() == &replacement.value_type
                                    && payload.as_ref() == function.result.as_ref())
                                && function.resources.mutations == [CheckedMutation::Element(0)]
                        }
                        _ => false,
                    })
                    .unwrap_or(CheckedType::Error),
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

    /// The reverse of `parameter_symbols[function_id]`: each parameter
    /// symbol mapped back to its position, for attributing a write found
    /// while walking `function_id`'s body back onto its own signature.
    fn parameter_positions(&self, function_id: FunctionId) -> HashMap<SymbolId, usize> {
        self.parameter_symbols
            .get(&function_id)
            .into_iter()
            .flatten()
            .enumerate()
            .filter_map(|(index, symbol)| symbol.map(|symbol| (symbol, index)))
            .collect()
    }

    /// The module a function is declared in, used as the calling context for
    /// call-site `mut` checks performed deep inside its body. Unlike
    /// `Item::Assignment`, an arbitrary expression's own syntax id is
    /// never in `syntax_modules` (only declarations and assignment
    /// statements are), so `module.module_for_syntax` cannot be applied
    /// directly to a `Call` node — this is resolved once per function
    /// instead, from a symbol that reliably carries it: any parameter (a
    /// parameter necessarily lives in the same module as the function
    /// itself), falling back to the function's own declaring binding.
    fn function_module(&self, module: &ResolvedModule, function: &ResolvedFunction) -> Option<ModuleId> {
        self.parameter_symbols
            .get(&function.id)
            .into_iter()
            .flatten()
            .flatten()
            .find_map(|symbol| module.symbol_module(*symbol))
            .or_else(|| {
                function
                    .binding_syntax
                    .and_then(|syntax| module.module_for_syntax(syntax))
            })
    }

    fn seed_function_types(&mut self, module: &ResolvedModule) {
        for function in module.functions() {
            let parameter_symbols = function_parameter_symbols(module, &function.pattern);
            let parameter_mutations = pattern_parameter_mutations(&function.pattern);
            if let Some(Type::Function(annotation)) = function.binding_annotation.as_ref() {
                let symbols = parameter_symbols.iter().flatten().copied().collect::<Vec<_>>();
                if let [symbol] = symbols.as_slice()
                    && let Some(id) = source_type_id(module, &annotation.parameter)
                {
                    self.symbol_companion_types.insert(*symbol, id);
                }
                if let Some(id) = source_type_id(module, &annotation.result) {
                    self.function_result_companion_types.insert(function.id, id);
                }
            }
            self.parameter_symbols
                .insert(function.id, parameter_symbols.clone());

            if let Some(expected) = self.impl_function_types.get(&function.id).cloned() {
                if !parameter_mutations.is_empty()
                    && parameter_mutations != expected.resources.mutations
                {
                    self.diagnostics.push(Diagnostic::new(
                        function.pattern.syntax().span.clone(),
                        format!(
                            "parameter `mut` markers declare {}, but the trait method declares {}",
                            format_named_resource_set(
                                &CheckedResourceSet::canonical_with_mutations(
                                    Vec::new(),
                                    parameter_mutations.clone(),
                                ),
                                &expected.parameter,
                            ),
                            format_named_resource_set(&expected.resources, &expected.parameter),
                        ),
                    ));
                }
                for symbol in mutation_parameter_symbols(
                    &parameter_symbols,
                    &expected.resources.mutations,
                ) {
                    self.mutable_parameter_symbols.insert(symbol);
                    self.mutated_parameter_symbols.insert(symbol);
                }
                self.function_types.insert(function.id, expected);
                if let Some(trait_id) = self.default_function_traits.get(&function.id).copied() {
                    let bounds = self.expand_trait_bounds(vec![CheckedTraitBound {
                        trait_id,
                        arguments: self.trait_parameter_arguments[&trait_id].clone(),
                    }]);
                    self.function_bounds.insert(function.id, bounds);
                }
                // A conditional trait implementation's own declared bounds
                // (e.g. `SomeBound T =>` in `impl T => SomeBound T => ...`)
                // are not covered by `default_function_traits` above — that
                // only handles a trait's *default* method bodies, which
                // rely on the trait's own prerequisites, not an
                // implementation's. Wire them the same way plain `def`s are
                // below, so an implementation member's body can rely on its
                // own header's bounds while being checked.
                let mut bounds = Vec::new();
                for bound in &function.trait_bounds {
                    if let Some(bound) = self.resolve_trait_bound(module, bound) {
                        bounds.push(bound);
                    }
                }
                if !bounds.is_empty() {
                    let bounds = self.expand_trait_bounds(bounds);
                    self.function_bounds
                        .entry(function.id)
                        .or_default()
                        .extend(bounds);
                }
                let mut subtype_bounds = Vec::new();
                for bound in &function.subtype_bounds {
                    if let Some(bound) = self.resolve_subtype_bound(module, bound) {
                        subtype_bounds.push(bound);
                    }
                }
                if !subtype_bounds.is_empty() {
                    self.function_subtype_bounds
                        .entry(function.id)
                        .or_default()
                        .extend(subtype_bounds);
                }
                continue;
            }
            let mut parameter = self.resolve_source_type(module, &function.pattern.ty());
            if !parameter.is_fully_known()
                && let Some(annotation) = &function.binding_annotation
                && let CheckedType::Function(annotation) =
                    self.resolve_source_type(module, annotation)
            {
                parameter = *annotation.parameter;
            }
            let mut result = function
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
            if !parameter.is_sized() && parameter != CheckedType::Error {
                self.diagnostics.push(Diagnostic::new(
                    function.pattern.syntax().span.clone(),
                    "function parameters must be sized",
                ));
                parameter = CheckedType::Error;
            }
            if !matches!(result, CheckedType::Inferred | CheckedType::Error) && !result.is_sized() {
                self.diagnostics.push(Diagnostic::new(
                    function
                        .result_annotation
                        .as_ref()
                        .map_or(Span::Compiler, |annotation| {
                            annotation.syntax().span.clone()
                        }),
                    "function results must be sized",
                ));
                result = CheckedType::Error;
            }
            let mut resources = function
                .binding_annotation
                .as_ref()
                .and_then(
                    |annotation| match self.resolve_source_type(module, annotation) {
                        CheckedType::Function(function) => Some(function.resources),
                        _ => None,
                    },
                )
                .unwrap_or_default();
            if !parameter_mutations.is_empty() {
                if function.binding_annotation.is_some()
                    && parameter_mutations != resources.mutations
                {
                    self.diagnostics.push(Diagnostic::new(
                        function.pattern.syntax().span.clone(),
                        format!(
                            "parameter `mut` markers declare {}, but the function annotation declares {}",
                            format_named_resource_set(
                                &CheckedResourceSet::canonical_with_mutations(
                                    Vec::new(),
                                    parameter_mutations.clone(),
                                ),
                                &parameter,
                            ),
                            format_named_resource_set(&resources, &parameter),
                        ),
                    ));
                } else {
                    resources.mutations = parameter_mutations.clone();
                }
            }
            for symbol in mutation_parameter_symbols(&parameter_symbols, &resources.mutations) {
                self.mutable_parameter_symbols.insert(symbol);
                self.mutated_parameter_symbols.insert(symbol);
            }
            let mut function_type = CheckedFunctionType {
                parameter: Box::new(parameter),
                resources,
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
            let mut bounds = Vec::new();
            for bound in &function.trait_bounds {
                if let Some(bound) = self.resolve_trait_bound(module, bound) {
                    bounds.push(bound);
                }
            }
            let bounds = self.expand_trait_bounds(bounds);
            if self.bounds_violate_functional_dependencies(&bounds) {
                self.diagnostics.push(Diagnostic::new(
                    function.pattern.syntax().span.clone(),
                    "trait bounds conflict with a functional dependency",
                ));
            }
            if !bounds.is_empty() {
                self.function_bounds.insert(function.id, bounds);
            }
            let mut subtype_bounds = Vec::new();
            for bound in &function.subtype_bounds {
                if let Some(bound) = self.resolve_subtype_bound(module, bound) {
                    subtype_bounds.push(bound);
                }
            }
            if !subtype_bounds.is_empty() {
                self.function_subtype_bounds.insert(function.id, subtype_bounds);
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
        if let Some(module_id) = self.function_module(module, &function) {
            self.checking_modules.push(module_id);
        }
        self.active_function_bounds.push(
            self.function_bounds
                .get(&function.id)
                .cloned()
                .unwrap_or_default(),
        );
        self.active_subtype_bounds.push(
            self.function_subtype_bounds
                .get(&function.id)
                .cloned()
                .unwrap_or_default(),
        );
        self.active_generic_parameters
            .push(self.declared_type_parameters(module, &function.type_parameters));
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
            resources: function_type.resources,
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
        self.active_subtype_bounds.pop();
        self.active_generic_parameters.pop();
        if self.function_module(module, &function).is_some() {
            self.checking_modules.pop();
        }
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

    /// Computes runtime resource requirements only. `target_parameters` is
    /// threaded through the existing traversal interface, but assignments,
    /// captures, and calls deliberately contribute no mutation effects.
    fn expression_resources_now(
        &self,
        module: &ResolvedModule,
        expression: &Expression,
        target_parameters: &HashMap<SymbolId, usize>,
    ) -> CheckedResourceSet {
        let union = |values: Vec<CheckedResourceSet>| {
            values
                .into_iter()
                .fold(CheckedResourceSet::default(), |resources, value| {
                    resources.union(&value)
                })
        };
        match expression {
            Expression::Function(_) => CheckedResourceSet::default(),
            Expression::Satisfies(value) => {
                self.expression_resources_now(module, &value.value, target_parameters)
            }
            Expression::Match(value) => union(
                std::iter::once(self.expression_resources_now(
                    module,
                    &value.subject,
                    target_parameters,
                ))
                .chain(value.arms.iter().map(|arm| {
                    self.expression_resources_now(module, &arm.body, target_parameters)
                }))
                .collect(),
            ),
            Expression::Loop(value) => {
                self.block_resources_now(module, &value.body, target_parameters)
            }
            Expression::Resource(value) => self
                .resource_types
                .get(&value.syntax.id)
                .cloned()
                .map(|resource| CheckedResourceSet::canonical(vec![resource]))
                .unwrap_or_default(),
            Expression::With(value) => {
                let provider =
                    self.expression_resources_now(module, &value.value, target_parameters);
                let body = self.block_resources_now(module, &value.body, target_parameters);
                let body = self
                    .resource_types
                    .get(&value.syntax.id)
                    .map_or(body.clone(), |resource| body.without(resource));
                provider.union(&body)
            }
            Expression::Block(value) => {
                self.block_resources_now(module, value, target_parameters)
            }
            Expression::Product(value) => union(
                value
                    .elements
                    .iter()
                    .map(|element| {
                        self.expression_resources_now(module, &element.value, target_parameters)
                    })
                    .collect(),
            ),
            Expression::Call(value) => {
                let mut result = self
                    .expression_resources_now(module, &value.callee, target_parameters)
                    .union(&self.expression_resources_now(
                        module,
                        &value.argument,
                        target_parameters,
                    ));
                // Prefer the callee sub-expression's own checked type over
                // the root function's full declared signature: for a
                // curried call `f a b`, `value.callee` is itself the call
                // `f a`, whose recorded type is already narrowed to the
                // *residual* function type (`f`'s type after applying `a`).
                // `function_origin` walks through every nesting level back
                // to `f` itself, so using `function_types.get` first would
                // reapply `f`'s outermost arrow's mutation/resource set at
                // every curry depth instead of just the matching one —
                // misattributing an early argument's `mut` effect onto a
                // later, unrelated argument.
                let called_type = match self.expression_types.get(&value.callee.syntax().id) {
                    Some(CheckedType::Function(function)) => Some(function.clone()),
                    _ => self
                        .function_origin(module, &value.callee)
                        .and_then(|function| self.function_types.get(&function).cloned()),
                };
                if let Some(function_type) = &called_type {
                    result = result.union(&CheckedResourceSet::canonical(
                        function_type.resources.resources.clone(),
                    ));
                }
                result
            }
            Expression::Access(value) => {
                self.expression_resources_now(module, &value.value, target_parameters)
            }
            Expression::Index(value) => self
                .expression_resources_now(module, &value.value, target_parameters)
                .union(&self.expression_resources_now(
                    module,
                    &value.index,
                    target_parameters,
                )),
            Expression::Logical(value) => self
                .expression_resources_now(module, &value.left, target_parameters)
                .union(&self.expression_resources_now(
                    module,
                    &value.right,
                    target_parameters,
                )),
            Expression::SyntaxArgument(_)
            | Expression::VisibilityArgument(_)
            | Expression::Quote(_)
            | Expression::Splice(_)
            | Expression::Name(_)
            | Expression::String(_)
            | Expression::CString(_)
            | Expression::Integer(_)
            | Expression::Float(_) => CheckedResourceSet::default(),
        }
    }

    /// `target_parameters` grows across the block's own statements, in
    /// order: a `let mut` binding whose value crosses a `Ref` rooted at a
    /// tracked parameter (or an already-tracked alias) makes the newly
    /// bound name an alias of that same position, so a later write through
    /// it — including via a helper function like `replace` — is correctly
    /// attributed back to the parameter. The extension is local to this
    /// block: it does not leak into the enclosing scope, but does propagate
    /// into nested blocks and closures the same way `target_parameters`
    /// itself already does, since they are handed the (possibly extended)
    /// map current at the point they are reached.
    fn block_resources_now(
        &self,
        module: &ResolvedModule,
        block: &crate::BlockExpression,
        target_parameters: &HashMap<SymbolId, usize>,
    ) -> CheckedResourceSet {
        let mut resources = CheckedResourceSet::default();
        for statement in &block.items {
            let current = target_parameters;
            let contribution = match statement {
                Item::Binding(value) => value
                    .value
                    .as_ref()
                    .map(|value| self.expression_resources_now(module, value, current))
                    .unwrap_or_default(),
                Item::PatternBinding(value) => {
                    self.expression_resources_now(module, &value.value, current)
                }
                Item::Assignment(value) => self
                    .expression_resources_now(module, &value.target, current)
                    .union(&self.expression_resources_now(module, &value.value, current)),
                Item::Return(value) => {
                    self.expression_resources_now(module, &value.value, current)
                }
                Item::Break(value) => value
                    .value
                    .as_ref()
                    .map(|value| self.expression_resources_now(module, value, current))
                    .unwrap_or_default(),
                Item::Continue(_) => CheckedResourceSet::default(),
                Item::Expression(value) => {
                    self.expression_resources_now(module, value, current)
                }
                Item::Submodule(_) => CheckedResourceSet::default(),
                Item::TypeDeclaration(_) => CheckedResourceSet::default(),
                Item::UseDeclaration(_) => CheckedResourceSet::default(),
                _ => CheckedResourceSet::default(),
            };
            resources = resources.union(&contribution);
        }
        resources
    }

    fn infer_resources(&mut self, module: &ResolvedModule) {
        let declared = module
            .functions()
            .iter()
            .filter_map(|function| {
                function.binding_annotation.as_ref().and_then(|annotation| {
                    matches!(annotation, Type::Function(_)).then(|| {
                        (
                            function.id,
                            self.function_types[&function.id].resources.clone(),
                        )
                    })
                })
            })
            .collect::<HashMap<_, _>>();

        for _ in 0..=module.functions().len() {
            let mut updates = Vec::new();
            for function in module.functions() {
                // An implementation member uses its trait method's complete
                // effect set as its callable ABI even when this particular
                // body exercises only a subset of those effects.
                if declared.contains_key(&function.id)
                    || self.impl_function_types.contains_key(&function.id)
                {
                    continue;
                }
                let target_parameters = self.parameter_positions(function.id);
                let inferred_body =
                    self.expression_resources_now(module, &function.body, &target_parameters);
                let inferred = CheckedResourceSet::canonical_with_mutations(
                    inferred_body.resources,
                    self.function_types[&function.id].resources.mutations.clone(),
                );
                if self.function_types[&function.id].resources != inferred {
                    updates.push((function.id, function.binding_syntax, inferred));
                }
            }
            let had_updates = !updates.is_empty();
            for (function, binding, resources) in updates {
                self.function_types
                    .get_mut(&function)
                    .expect("function type")
                    .resources = resources.clone();
                if let Some(syntax) = binding
                    && let Some(symbol) = module.symbol_for(syntax)
                    && let Some(CheckedType::Function(function_type)) =
                        self.symbol_types.get_mut(&symbol)
                {
                    function_type.resources = resources;
                }
            }
            let refreshed = self.refresh_function_value_types(module);
            if !had_updates && !refreshed {
                break;
            }
        }

        for function in module.functions() {
            let target_parameters = self.parameter_positions(function.id);
            let actual_body =
                self.expression_resources_now(module, &function.body, &target_parameters);
            let actual = CheckedResourceSet::canonical(actual_body.resources);
            if let Some(allowed) = declared.get(&function.id)
                && !actual.is_subset_of(allowed)
            {
                let parameter = self.function_types[&function.id].parameter.clone();
                self.diagnostics.push(Diagnostic::new(
                    function.body.syntax().span.clone(),
                    format!(
                        "function body requires effects {}, which are not contained in its \
                         declared effect set {}",
                        format_named_resource_set(&actual, &parameter),
                        format_named_resource_set(allowed, &parameter)
                    ),
                ));
            } else if let Some(trait_type) = self.impl_function_types.get(&function.id)
                && !actual.is_subset_of(&trait_type.resources)
            {
                // Impl methods are resolved without an expected type of
                // their own (`binding_annotation` is always `None`), so
                // they are never in `declared` and their inferred effects
                // would otherwise silently overwrite the trait's — this is
                // the one place that keeps an implementation honest against
                // what its trait method promised callers.
                let parameter = trait_type.parameter.clone();
                let allowed = trait_type.resources.clone();
                self.diagnostics.push(Diagnostic::new(
                    function.body.syntax().span.clone(),
                    format!(
                        "trait implementation requires effects {}, which are not contained in \
                         the trait method's declared effect set {}",
                        format_named_resource_set(&actual, &parameter),
                        format_named_resource_set(&allowed, &parameter)
                    ),
                ));
            }
            let current_module = self.function_module(module, function);
            self.record_expression_resources(
                module,
                &function.body,
                &target_parameters,
                current_module,
            );
        }

        let no_parameters = HashMap::new();
        let entry_module = module.program().entry();
        for source_module in module.program().modules() {
            let is_entry_module = source_module.id == entry_module;
            for item in &source_module.syntax.items {
                let (syntax, resources) = match item {
                    Item::Binding(binding) => binding.value.as_ref().map(|value| {
                        (
                            value.syntax(),
                            self.expression_resources_now(module, value, &no_parameters),
                        )
                    }),
                    Item::PatternBinding(binding) => Some((
                        binding.value.syntax(),
                        self.expression_resources_now(module, &binding.value, &no_parameters),
                    )),
                    Item::Assignment(value) => Some((
                        &value.syntax,
                        self.expression_resources_now(module, &value.target, &no_parameters)
                            .union(&self.expression_resources_now(
                                module,
                                &value.value,
                                &no_parameters,
                            )),
                    )),
                    Item::Return(value) => Some((
                        &value.syntax,
                        self.expression_resources_now(module, &value.value, &no_parameters),
                    )),
                    Item::Break(value) => value.value.as_ref().map(|expression| {
                        (
                            &value.syntax,
                            self.expression_resources_now(module, expression, &no_parameters),
                        )
                    }),
                    Item::Continue(_) => None,
                    Item::Expression(value) => Some((
                        value.syntax(),
                        self.expression_resources_now(module, value, &no_parameters),
                    )),
                    Item::Submodule(_) => None,
                    Item::TypeDeclaration(_) => None,
                    Item::UseDeclaration(_) => None,
                    _ => None,
                }
                .unwrap_or((&source_module.syntax.syntax, CheckedResourceSet::default()));
                let required = if is_entry_module {
                    CheckedResourceSet::canonical_with_mutations(
                        resources
                            .resources
                            .iter()
                            .filter(|resource| {
                                !matches!(
                                    &resource.value_type,
                                    CheckedType::Opaque { id, .. } if Some(*id) == self.io_type
                                )
                            })
                            .cloned()
                            .collect(),
                        resources.mutations.clone(),
                    )
                } else {
                    resources
                };
                if !required.resources.is_empty() {
                    self.diagnostics.push(Diagnostic::new(
                        syntax.span.clone(),
                        format!("top-level initialization requires resources {required}"),
                    ));
                }
            }
        }
    }

    fn refreshed_expression_type(
        &self,
        module: &ResolvedModule,
        expression: &Expression,
    ) -> Option<CheckedType> {
        match expression {
            Expression::Function(function) => module
                .function_for(function.syntax.id)
                .and_then(|id| self.function_types.get(&id).cloned())
                .map(CheckedType::Function),
            Expression::Satisfies(_) => self.expression_types.get(&expression.syntax().id).cloned(),
            Expression::With(with) => self.refreshed_block_type(module, &with.body),
            Expression::Block(block) => self.refreshed_block_type(module, block),
            Expression::Call(call) => self
                .refreshed_expression_type(module, &call.callee)
                .and_then(|ty| match ty {
                    CheckedType::Function(function) => Some(*function.result),
                    _ => None,
                })
                .or_else(|| self.expression_types.get(&expression.syntax().id).cloned()),
            Expression::Name(_) | Expression::Access(_) => {
                let refreshed = module
                    .symbol_for(expression.syntax().id)
                    .and_then(|symbol| self.symbol_types.get(&symbol).cloned());
                let existing = self.expression_types.get(&expression.syntax().id).cloned();
                match (refreshed, existing) {
                    (
                        Some(CheckedType::Function(template)),
                        Some(CheckedType::Function(instantiated)),
                    ) => {
                        let template = CheckedType::Function(template);
                        let instantiated = CheckedType::Function(instantiated);
                        let mut inference_template = template.clone();
                        let mut inference_actual = instantiated.clone();
                        clear_function_resources(&mut inference_template);
                        clear_function_resources(&mut inference_actual);
                        let mut substitutions = HashMap::new();
                        if infer_type_parameters(
                            &inference_template,
                            &inference_actual,
                            &mut substitutions,
                        ) {
                            Some(substitute_type(template, &substitutions))
                        } else {
                            Some(instantiated)
                        }
                    }
                    (Some(refreshed), _) => Some(refreshed),
                    (None, existing) => existing,
                }
            }
            _ => self.expression_types.get(&expression.syntax().id).cloned(),
        }
    }

    fn refreshed_block_type(
        &self,
        module: &ResolvedModule,
        block: &crate::BlockExpression,
    ) -> Option<CheckedType> {
        match block.items.last()? {
            Item::Expression(expression) => self.refreshed_expression_type(module, expression),
            Item::Return(value) => self.refreshed_expression_type(module, &value.value),
            _ => None,
        }
    }

    fn refresh_function_value_types(&mut self, module: &ResolvedModule) -> bool {
        let mut changed = false;
        for function in module.functions() {
            if function.binding_annotation.is_none()
                && function.result_annotation.is_none()
                && let Some(result) = self.refreshed_expression_type(module, &function.body)
                && let Some(function_type) = self.function_types.get_mut(&function.id)
                && matches!(function_type.result.as_ref(), CheckedType::Function(_))
                && matches!(&result, CheckedType::Function(_))
                && function_type.result.as_ref() != &result
            {
                function_type.result = Box::new(result);
                changed = true;
            }
            if let Some(binding) = function.binding_syntax
                && let Some(symbol) = module.symbol_for(binding)
                && let Some(function_type) = self.function_types.get(&function.id).cloned()
            {
                self.symbol_types
                    .insert(symbol, CheckedType::Function(function_type));
            }
        }

        for source_module in module.program().modules() {
            for item in &source_module.syntax.items {
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
                    changed |= self.refresh_statement_function_types(module, item);
                }
            }
        }
        let bodies = module
            .functions()
            .iter()
            .map(|function| function.body.clone())
            .collect::<Vec<_>>();
        for body in &bodies {
            changed |= self.refresh_expression_function_types(module, body);
        }
        changed
    }

    fn refresh_statement_function_types(
        &mut self,
        module: &ResolvedModule,
        statement: &Item,
    ) -> bool {
        match statement {
            Item::Binding(binding) => {
                let mut changed = false;
                if let Some(value) = &binding.value {
                    changed |= self.refresh_expression_function_types(module, value);
                    if let Some(symbol) = module.symbol_for(binding.syntax.id)
                        && !self.function_symbols.contains_key(&symbol)
                        && let Some(value_type) = self.refreshed_expression_type(module, value)
                        && matches!(&value_type, CheckedType::Function(_))
                        && matches!(
                            self.symbol_types.get(&symbol),
                            Some(CheckedType::Function(_))
                        )
                        && self.symbol_types.get(&symbol) != Some(&value_type)
                    {
                        self.symbol_types.insert(symbol, value_type);
                        changed = true;
                    }
                }
                changed
            }
            Item::PatternBinding(binding) => {
                self.refresh_expression_function_types(module, &binding.value)
            }
            Item::Assignment(value) => {
                self.refresh_expression_function_types(module, &value.target)
                    | self.refresh_expression_function_types(module, &value.value)
            }
            Item::Return(value) => {
                self.refresh_expression_function_types(module, &value.value)
            }
            Item::Break(value) => value
                .value
                .as_ref()
                .is_some_and(|value| self.refresh_expression_function_types(module, value)),
            Item::Continue(_) => false,
            Item::Expression(value) => self.refresh_expression_function_types(module, value),
            Item::Submodule(_) => false,
            Item::TypeDeclaration(_) => false,
            Item::UseDeclaration(_) => false,
            _ => false,
        }
    }

    fn refresh_expression_function_types(
        &mut self,
        module: &ResolvedModule,
        expression: &Expression,
    ) -> bool {
        let mut changed = match expression {
            Expression::Function(function) => {
                self.refresh_expression_function_types(module, &function.body)
            }
            Expression::Satisfies(value) => {
                self.refresh_expression_function_types(module, &value.value)
            }
            Expression::Match(value) => {
                let mut changed = self.refresh_expression_function_types(module, &value.subject);
                for arm in &value.arms {
                    changed |= self.refresh_expression_function_types(module, &arm.body);
                }
                changed
            }
            Expression::Loop(value) => {
                value
                    .body
                    .items
                    .iter()
                    .fold(false, |changed, statement| {
                        self.refresh_statement_function_types(module, statement) | changed
                    })
            }
            Expression::With(value) => {
                let mut changed = self.refresh_expression_function_types(module, &value.value);
                for statement in &value.body.items {
                    changed |= self.refresh_statement_function_types(module, statement);
                }
                changed
            }
            Expression::Block(value) => {
                value.items.iter().fold(false, |changed, statement| {
                    self.refresh_statement_function_types(module, statement) | changed
                })
            }
            Expression::Product(value) => value.elements.iter().fold(false, |changed, element| {
                self.refresh_expression_function_types(module, &element.value) | changed
            }),
            Expression::Call(value) => {
                self.refresh_expression_function_types(module, &value.callee)
                    | self.refresh_expression_function_types(module, &value.argument)
            }
            Expression::Access(value) => {
                self.refresh_expression_function_types(module, &value.value)
            }
            Expression::Index(value) => {
                self.refresh_expression_function_types(module, &value.value)
                    | self.refresh_expression_function_types(module, &value.index)
            }
            Expression::Logical(value) => {
                self.refresh_expression_function_types(module, &value.left)
                    | self.refresh_expression_function_types(module, &value.right)
            }
            Expression::Resource(_)
            | Expression::SyntaxArgument(_)
            | Expression::VisibilityArgument(_)
            | Expression::Quote(_)
            | Expression::Splice(_)
            | Expression::Name(_)
            | Expression::String(_)
            | Expression::CString(_)
            | Expression::Integer(_)
            | Expression::Float(_) => false,
        };
        if matches!(
            self.expression_types.get(&expression.syntax().id),
            Some(CheckedType::Function(_))
        ) && let Some(value_type) = self.refreshed_expression_type(module, expression)
            && matches!(&value_type, CheckedType::Function(_))
            && self.expression_types.get(&expression.syntax().id) != Some(&value_type)
        {
            self.expression_types
                .insert(expression.syntax().id, value_type);
            changed = true;
        }
        changed
    }

    fn record_expression_resources(
        &mut self,
        module: &ResolvedModule,
        expression: &Expression,
        target_parameters: &HashMap<SymbolId, usize>,
        current_module: Option<ModuleId>,
    ) {
        // Backfills a bare function reference's resources once an
        // unannotated callee's effects have settled (see the comment on the
        // `Expression::Call` arm below). Restricted to a bare reference
        // (curry depth 0): `function_origin` walks straight through any
        // number of `Call` wrappers to the same root function, but a
        // partially-applied `Call` node's own recorded type is already the
        // *residual* function type for its specific curry depth, correctly
        // narrowed during ordinary checking. Backfilling it with the root's
        // full (first-arrow) resources would reapply an early argument's
        // effects at every later curry depth too.
        if !matches!(expression, Expression::Call(_))
            && let Some(function_id) = self.function_origin(module, expression)
            && let Some(resources) = self
                .function_types
                .get(&function_id)
                .map(|function| function.resources.clone())
            && let Some(CheckedType::Function(function)) =
                self.expression_types.get_mut(&expression.syntax().id)
        {
            function.resources = resources;
        }
        let resources = self.expression_resources_now(module, expression, target_parameters);
        self.expression_resources
            .insert(expression.syntax().id, resources);
        match expression {
            // A nested function literal's own syntax nodes are recorded
            // separately, on its own pass over `module.functions()`.
            Expression::Function(_) | Expression::Resource(_) => {}
            Expression::Satisfies(value) => self.record_expression_resources(
                module,
                &value.value,
                target_parameters,
                current_module,
            ),
            Expression::Match(value) => {
                self.record_expression_resources(
                    module,
                    &value.subject,
                    target_parameters,
                    current_module,
                );
                for arm in &value.arms {
                    self.record_expression_resources(
                        module,
                        &arm.body,
                        target_parameters,
                        current_module,
                    );
                }
            }
            Expression::Loop(value) => {
                for statement in &value.body.items {
                    self.record_statement_resources(
                        module,
                        statement,
                        target_parameters,
                        current_module,
                    );
                }
            }
            Expression::With(value) => {
                self.record_expression_resources(
                    module,
                    &value.value,
                    target_parameters,
                    current_module,
                );
                for statement in &value.body.items {
                    self.record_statement_resources(
                        module,
                        statement,
                        target_parameters,
                        current_module,
                    );
                }
            }
            Expression::Block(value) => {
                for statement in &value.items {
                    self.record_statement_resources(
                        module,
                        statement,
                        target_parameters,
                        current_module,
                    );
                }
            }
            Expression::Product(value) => {
                for element in &value.elements {
                    self.record_expression_resources(
                        module,
                        &element.value,
                        target_parameters,
                        current_module,
                    );
                }
            }
            Expression::Call(value) => {
                self.record_expression_resources(
                    module,
                    &value.callee,
                    target_parameters,
                    current_module,
                );
                self.record_expression_resources(
                    module,
                    &value.argument,
                    target_parameters,
                    current_module,
                );
                // Prefer the callee sub-expression's own checked type over
                // the root function's full declared signature: for a
                // curried call `f a b`, `value.callee` is itself the call
                // `f a`, whose recorded type is already narrowed to the
                // *residual* function type. `function_origin` walks through
                // every nesting level back to `f`, so using
                // `function_types.get` first would reapply `f`'s outermost
                // arrow's mutation set at every curry depth instead of just
                // the matching one (see the identical fix in
                // `expression_resources_now`'s `Expression::Call` arm).
                let called_type = match self.expression_types.get(&value.callee.syntax().id) {
                    Some(CheckedType::Function(function)) => Some(function.clone()),
                    _ => self
                        .function_origin(module, &value.callee)
                        .and_then(|function| self.function_types.get(&function).cloned()),
                };
                if let Some(function_type) = called_type
                    && !function_type.resources.mutations.is_empty()
                {
                    self.check_call_mutations(
                        module,
                        &value.argument,
                        &function_type.resources.mutations,
                        current_module,
                    );
                }
            }
            Expression::Access(value) => self.record_expression_resources(
                module,
                &value.value,
                target_parameters,
                current_module,
            ),
            Expression::Index(value) => {
                self.record_expression_resources(
                    module,
                    &value.value,
                    target_parameters,
                    current_module,
                );
                self.record_expression_resources(
                    module,
                    &value.index,
                    target_parameters,
                    current_module,
                );
            }
            Expression::Logical(value) => {
                self.record_expression_resources(
                    module,
                    &value.left,
                    target_parameters,
                    current_module,
                );
                self.record_expression_resources(
                    module,
                    &value.right,
                    target_parameters,
                    current_module,
                );
            }
            Expression::SyntaxArgument(_)
            | Expression::VisibilityArgument(_)
            | Expression::Quote(_)
            | Expression::Splice(_)
            | Expression::Name(_)
            | Expression::String(_)
            | Expression::CString(_)
            | Expression::Integer(_)
            | Expression::Float(_) => {}
        }
    }

    fn record_statement_resources(
        &mut self,
        module: &ResolvedModule,
        statement: &Item,
        target_parameters: &HashMap<SymbolId, usize>,
        current_module: Option<ModuleId>,
    ) {
        match statement {
            Item::Binding(value) => {
                if let Some(value) = &value.value {
                    self.record_expression_resources(
                        module,
                        value,
                        target_parameters,
                        current_module,
                    );
                }
            }
            Item::PatternBinding(value) => self.record_expression_resources(
                module,
                &value.value,
                target_parameters,
                current_module,
            ),
            Item::Assignment(value) => {
                self.record_expression_resources(
                    module,
                    &value.target,
                    target_parameters,
                    current_module,
                );
                self.record_expression_resources(
                    module,
                    &value.value,
                    target_parameters,
                    current_module,
                );
            }
            Item::Return(value) => self.record_expression_resources(
                module,
                &value.value,
                target_parameters,
                current_module,
            ),
            Item::Break(value) => {
                if let Some(value) = &value.value {
                    self.record_expression_resources(
                        module,
                        value,
                        target_parameters,
                        current_module,
                    );
                }
            }
            Item::Continue(_) => {}
            Item::Expression(value) => self.record_expression_resources(
                module,
                value,
                target_parameters,
                current_module,
            ),
            Item::Submodule(_) => {}
            Item::TypeDeclaration(_) => {}
            Item::UseDeclaration(_) => {}
            _ => {}
        }
    }

    fn require_copy_at_pattern(&mut self, pattern: &crate::AtPattern, value_type: &CheckedType) {
        let bounds = self
            .active_function_bounds
            .iter()
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        if !is_copy_type(
            value_type,
            self.copy_trait,
            self.drop_trait,
            self.io_type,
            &self.trait_implementations,
            &bounds,
        ) {
            self.diagnostics.push(Diagnostic::new(
                pattern.syntax.span.clone(),
                format!("an `@` pattern requires a Copy value, found `{value_type}`"),
            ));
        }
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
            Pattern::At(at) => {
                self.bind_pattern_types(
                    module,
                    &Pattern::Binding(at.binding.as_ref().clone()),
                    value_type,
                );
                self.require_copy_at_pattern(at, value_type);
                self.bind_pattern_types(module, &at.pattern, value_type);
            }
            Pattern::Wildcard(wildcard) => {
                if !matches!(wildcard.ty, Type::Inferred(_)) {
                    let declared = self.resolve_source_type(module, &wildcard.ty);
                    self.require_compatible(
                        value_type.clone(),
                        declared,
                        wildcard.syntax.span.clone(),
                    );
                }
            }
            Pattern::StringLiteral(pattern) => {
                let Ok(value) = crate::string_literal::decode(&pattern.literal) else {
                    return;
                };
                if !matches!(value_type, CheckedType::StringLiteralSet(values) if values.as_slice() == [value])
                {
                    self.diagnostics.push(Diagnostic::new(
                        pattern.syntax.span.clone(),
                        "a string literal binding pattern must have the same singleton literal type",
                    ));
                }
            }
            Pattern::Binding(binding) => {
                if let Some(expected_id) = module.type_for_pattern(binding.syntax.id) {
                    if !matches!(value_type, CheckedType::Distinct { id, .. } if *id == expected_id)
                        && *value_type != CheckedType::Error
                    {
                        self.diagnostics.push(Diagnostic::new(
                            binding.syntax.span.clone(),
                            format!(
                                "singleton pattern `{}` cannot match `{value_type}`",
                                binding.name
                            ),
                        ));
                    }
                    return;
                }
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
                    CheckedType::String
                        if module.builtin_type(expected_id) == Some(BuiltinType::String) =>
                    {
                        let representation = self
                            .string_representation
                            .clone()
                            .unwrap_or_else(expected_string_representation);
                        self.bind_pattern_types(module, &pattern.argument, &representation);
                    }
                    CheckedType::Ref(payload)
                        if module.builtin_type(expected_id) == Some(BuiltinType::Ref) =>
                    {
                        if matches!(payload.as_ref(), CheckedType::ErasedProduct(_)) {
                            self.diagnostics.push(Diagnostic::new(
                                pattern.syntax.span.clone(),
                                "an erased product reference cannot be destructured",
                            ));
                            return;
                        }
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
            Pattern::Splice(_) => {}
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
                        if external_type
                            .as_ref()
                            .is_some_and(checked_type_contains_erased_product)
                        {
                            self.diagnostics.push(Diagnostic::new(
                                binding.syntax.span.clone(),
                                "external binding types cannot contain erased products",
                            ));
                        }
                        if matches!(
                            &external_type,
                            Some(CheckedType::Function(CheckedFunctionType { result, .. }))
                                if checked_type_contains_cstring(&result)
                        ) {
                            self.diagnostics.push(Diagnostic::new(
                                binding.syntax.span.clone(),
                                "external functions cannot return owned `CString` values",
                            ));
                        }
                        if matches!(
                            &external_type,
                            Some(CheckedType::Function(CheckedFunctionType { resources, .. }))
                                if !resources.resources.is_empty()
                        ) {
                            self.diagnostics.push(Diagnostic::new(
                                binding.syntax.span.clone(),
                                "external functions cannot require Staple resources",
                            ));
                        }
                    }
                    self.check_binding(module, binding);
                }
            }
            statement @ (Item::Binding(_)
            | Item::PatternBinding(_)
            | Item::Assignment(_)
            | Item::Return(_)
            | Item::Break(_)
            | Item::Continue(_)
            | Item::Expression(_)) => {
                self.check_statement(module, statement);
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

    fn check_statement(&mut self, module: &ResolvedModule, statement: &Item) -> CheckedType {
        match statement {
            Item::Binding(binding) => {
                self.check_binding(module, binding);
                CheckedType::empty_product()
            }
            Item::PatternBinding(binding) => {
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
            Item::Assignment(assignment) => {
                if let Expression::Index(index) = &assignment.target {
                    let target_type = self.check_expression(module, &index.value);
                    let mut position_type = self.check_expression(module, &index.index);
                    let Some(trait_id) = self.mutate_index_trait else {
                        self.diagnostics.push(Diagnostic::new(
                            assignment.syntax.span.clone(),
                            "standard-library trait `MutateIndex` is unavailable",
                        ));
                        return CheckedType::empty_product();
                    };
                    let mut partial =
                        vec![target_type, position_type.clone(), CheckedType::Inferred];
                    let mut resolved = self.resolve_trait_obligation(trait_id, &partial);
                    if resolved.is_none()
                        && matches!(index.index.as_ref(), Expression::Integer(_))
                        && position_type != CheckedType::USize
                    {
                        position_type = self.check_expression_expected(
                            module,
                            &index.index,
                            Some(&CheckedType::USize),
                        );
                        partial[1] = position_type;
                        resolved = self.resolve_trait_obligation(trait_id, &partial);
                    }
                    let Some(arguments) = resolved else {
                        self.diagnostics.push(Diagnostic::new(
                            assignment.target.syntax().span.clone(),
                            "indexed assignment requires a `MutateIndex` implementation",
                        ));
                        self.check_expression(module, &assignment.value);
                        return CheckedType::empty_product();
                    };
                    if let Expression::Integer(literal) = index.index.as_ref()
                        && literal
                            .literal
                            .parse::<usize>()
                            .ok()
                            .is_some_and(|position| {
                                structural_index_length(&arguments[0])
                                    .is_some_and(|length| position >= length)
                            })
                    {
                        self.diagnostics.push(Diagnostic::new(
                            literal.syntax.span.clone(),
                            format!("product index `{}` is out of bounds", literal.literal),
                        ));
                    }
                    self.check_expression_expected(module, &assignment.value, Some(&arguments[2]));
                    // `mutate_index`'s fixed signature declares `mut 0` on
                    // `Target`; `index.value` occupies that position. This
                    // does not depend on inference having run: the target
                    // is compiler-fixed, not read from `self.function_types`.
                    let current_module = module.module_for_syntax(assignment.syntax.id);
                    self.check_call_mutations(
                        module,
                        &index.value,
                        &[CheckedMutation::Element(0)],
                        current_module,
                    );
                    if let Some(method) = module
                        .traits()
                        .get(&trait_id)
                        .and_then(|resolved| resolved.methods.first())
                        .copied()
                    {
                        self.trait_dispatches.insert(
                            assignment.syntax.id,
                            CheckedTraitDispatch { method, arguments },
                        );
                    }
                    return CheckedType::empty_product();
                }
                let target_type = self.check_expression(module, &assignment.target);
                if !self.did_return {
                    self.check_assignment_place(module, assignment);
                    self.check_expression_expected(module, &assignment.value, Some(&target_type));
                }
                CheckedType::empty_product()
            }
            Item::Return(statement) => {
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
            Item::Break(statement) => {
                let expected = self
                    .loop_contexts
                    .last()
                    .and_then(|context| context.expected.clone());
                let contribution = if let Some(value) = &statement.value {
                    let value_type =
                        self.check_expression_expected(module, value, expected.as_ref());
                    ReturnContribution {
                        syntax: Some(value.syntax().id),
                        span: value.syntax().span.clone(),
                        value_type,
                    }
                } else {
                    let value_type = if let Some(expected) = expected {
                        self.require_compatible(
                            CheckedType::empty_product(),
                            expected,
                            statement.syntax.span.clone(),
                        )
                    } else {
                        CheckedType::empty_product()
                    };
                    ReturnContribution {
                        syntax: None,
                        span: statement.syntax.span.clone(),
                        value_type,
                    }
                };
                if !self.did_return && self.return_reachable {
                    self.loop_contexts
                        .last_mut()
                        .expect("break inside loop")
                        .breaks
                        .push(contribution);
                    self.did_return = true;
                }
                CheckedType::empty_product()
            }
            Item::Continue(_) => {
                if !self.did_return && self.return_reachable {
                    self.did_return = true;
                }
                CheckedType::empty_product()
            }
            Item::Expression(expression) => self.check_expression(module, expression),
            Item::Submodule(_) => CheckedType::empty_product(),
            Item::TypeDeclaration(_) => CheckedType::empty_product(),
            Item::UseDeclaration(_) => CheckedType::empty_product(),
            _ => CheckedType::empty_product(),
        }
    }

    fn check_assignment_place(&mut self, module: &ResolvedModule, assignment: &crate::Assignment) {
        let current_module = module.module_for_syntax(assignment.syntax.id);
        if let Some(issue) =
            self.writable_place_issue(module, &assignment.target, current_module, false, false)
        {
            let message = match issue {
                PlaceIssue::NotReassignable(name) => {
                    format!("cannot reassign `{name}`; its binding is not declared `mut`")
                }
                PlaceIssue::NotMutable(name) => {
                    format!("cannot write through `{name}`; its binding is not declared `mut`")
                }
                PlaceIssue::NotAPlace => "assignment target is not a writable place".to_string(),
            };
            self.diagnostics
                .push(Diagnostic::new(assignment.target.syntax().span.clone(), message));
        }
    }

    /// Checks whether `expression` may appear as an assignment target.
    ///
    /// `projected` is `true` once the walk has crossed at least one field or
    /// index access away from the root binding: both rebinding the root and
    /// writing through a projection into an already-owned value require the
    /// same `mut` flag on the root. Function parameters are provisionally
    /// writable while their body is checked; inference and declared effects
    /// subsequently constrain which parameter positions may be changed.
    fn writable_place_issue(
        &self,
        module: &ResolvedModule,
        expression: &Expression,
        current_module: Option<ModuleId>,
        projected: bool,
        _crossed_ref: bool,
    ) -> Option<PlaceIssue> {
        if let Some(symbol) = module.symbol_for(expression.syntax().id) {
            let is_parameter = self.mutable_parameter_symbols.contains(&symbol);
            let permitted = is_parameter || module.is_mutable_symbol(symbol);
            if permitted && module.symbol_module(symbol) == current_module {
                return None;
            }
            if !permitted {
                let name = place_expression_name(expression);
                return Some(if !projected {
                    PlaceIssue::NotReassignable(name)
                } else {
                    PlaceIssue::NotMutable(name)
                });
            }
            return Some(PlaceIssue::NotAPlace);
        }
        match expression {
            Expression::Access(access) => {
                self.writable_place_issue(module, &access.value, current_module, true, false)
            }
            Expression::Index(index) => {
                self.writable_place_issue(module, &index.value, current_module, true, false)
            }
            Expression::Name(_) => Some(PlaceIssue::NotAPlace),
            other => {
                if projected && self.expression_crosses_ref(other) {
                    None
                } else {
                    Some(PlaceIssue::NotAPlace)
                }
            }
        }
    }

    fn expression_crosses_ref(&self, expression: &Expression) -> bool {
        let Some(mut value_type) = self.expression_types.get(&expression.syntax().id).cloned()
        else {
            return false;
        };
        loop {
            match value_type {
                CheckedType::Distinct { representation, .. } => value_type = *representation,
                CheckedType::Ref(_) => return true,
                _ => return false,
            }
        }
    }

    /// Requires that a call's argument roots are writable wherever the
    /// callee declares a `mut` effect on the corresponding parameter
    /// position. A `let` binding otherwise gives no real guarantee — a
    /// callee could freely mutate through it — so this is checked exactly
    /// like `check_assignment_place`, just at the boundary of a call
    /// instead of a literal assignment statement. An argument with no root
    /// place (a temporary) is never checked: nothing holds a reference to
    /// it that mutation could surprise.
    fn check_call_mutations(
        &mut self,
        module: &ResolvedModule,
        argument: &Expression,
        mutations: &[CheckedMutation],
        current_module: Option<ModuleId>,
    ) {
        for mutation in mutations {
            let target_expression = call_mutation_target_expression(argument, mutation);
            let Some((root, _)) = place_root_symbol(module, target_expression) else {
                continue;
            };
            // When the calling context's module could not be determined
            // (an anonymous closure with no parameters and no binding to
            // anchor on), the module boundary is not enforced; it only ever
            // protects a `pub` global from another module, which cannot
            // apply to a symbol reached from an unnamed closure regardless.
            let permitted = (module.is_mutable_symbol(root)
                || self.mutable_parameter_symbols.contains(&root))
                && (current_module.is_none() || module.symbol_module(root) == current_module);
            if !permitted {
                let name = place_expression_name(target_expression);
                self.diagnostics.push(Diagnostic::new(
                    target_expression.syntax().span.clone(),
                    format!("cannot write through `{name}`; its binding is not declared `mut`"),
                ));
            }
        }
    }

    fn check_propagating_binding(
        &mut self,
        module: &ResolvedModule,
        binding: &crate::PatternBinding,
        value_type: &CheckedType,
    ) {
        let mut root = &binding.pattern;
        while let Pattern::At(at) = root {
            self.pattern_types.insert(at.syntax.id, value_type.clone());
            self.bind_pattern_types(
                module,
                &Pattern::Binding(at.binding.as_ref().clone()),
                value_type,
            );
            self.require_copy_at_pattern(at, value_type);
            root = &at.pattern;
        }
        let Pattern::Nominal(pattern) = root else {
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
        if !checked_type.is_sized() && checked_type != CheckedType::Error {
            self.diagnostics.push(Diagnostic::new(
                binding.syntax.span.clone(),
                format!("binding `{}` has an unsized type", binding.name),
            ));
        }
        if !checked_type.is_fully_known() && checked_type != CheckedType::Error {
            self.diagnostics.push(Diagnostic::new(
                binding.syntax.span.clone(),
                format!("could not fully infer the type of `{}`", binding.name),
            ));
        }
        let has_free_type_parameter = contains_type_parameter(&checked_type) && {
            let bound_parameters: HashSet<TypeParameterId> = self
                .active_generic_parameters
                .iter()
                .flatten()
                .copied()
                .collect();
            type_parameter_ids(&checked_type)
                .iter()
                .any(|id| !bound_parameters.contains(id))
        };
        if binding.type_parameters.is_empty()
            && has_free_type_parameter
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

    /// Checks a product expression containing one or more `...=` spreads.
    /// Named spreads merge their operand's elements by name rather than by
    /// position, so the result can only be laid out once the expected named
    /// product type is known; later elements (explicit or spread) override
    /// earlier ones with the same name.
    fn check_named_spread_product(
        &mut self,
        module: &ResolvedModule,
        product: &ProductExpression,
        expected: Option<&CheckedType>,
    ) -> CheckedType {
        let expected_product = match expected {
            Some(CheckedType::Product(expected_product))
                if !expected_product.variadic
                    && expected_product
                        .elements
                        .iter()
                        .all(|element| element.name.is_some()) =>
            {
                Some(expected_product.clone())
            }
            _ => None,
        };
        if expected_product.is_none() {
            self.diagnostics.push(Diagnostic::new(
                product.syntax.span.clone(),
                "a named product spread (`...=`) requires a fully-named expected product type",
            ));
        }

        let mut fields: HashMap<String, (CheckedType, Span)> = HashMap::new();
        for element in &product.elements {
            if element.spread {
                let value_type = self.check_expression(module, &element.value);
                if self.did_return {
                    return CheckedType::empty_product();
                }
                if !element.named_spread {
                    self.diagnostics.push(Diagnostic::new(
                        element.syntax.span.clone(),
                        "cannot combine a positional spread with a named spread in the same product",
                    ));
                    continue;
                }
                match value_type {
                    CheckedType::Product(operand) if !operand.variadic => {
                        if operand.elements.iter().any(|field| field.name.is_none()) {
                            self.diagnostics.push(Diagnostic::new(
                                element.syntax.span.clone(),
                                "a named spread operand must have every element named",
                            ));
                        } else {
                            for field in operand.elements {
                                fields.insert(
                                    field.name.expect("checked all-named above"),
                                    (field.value_type, element.syntax.span.clone()),
                                );
                            }
                        }
                    }
                    CheckedType::ErasedProduct(_) => {
                        self.diagnostics.push(Diagnostic::new(
                            element.syntax.span.clone(),
                            "cannot spread an erased product value",
                        ));
                    }
                    CheckedType::Error => {}
                    other => self.diagnostics.push(Diagnostic::new(
                        element.syntax.span.clone(),
                        format!("cannot spread non-product value of type `{other}`"),
                    )),
                }
                continue;
            }
            let Some(name) = element.name.clone() else {
                self.diagnostics.push(Diagnostic::new(
                    element.syntax.span.clone(),
                    "every element must be named when the product contains a named spread",
                ));
                self.check_expression(module, &element.value);
                if self.did_return {
                    return CheckedType::empty_product();
                }
                continue;
            };
            let expected_field_type = expected_product.as_ref().and_then(|expected_product| {
                expected_product
                    .elements
                    .iter()
                    .find(|field| field.name.as_deref() == Some(name.as_str()))
                    .map(|field| &field.value_type)
            });
            let value_type =
                self.check_expression_expected(module, &element.value, expected_field_type);
            if self.did_return {
                return CheckedType::empty_product();
            }
            fields.insert(name, (value_type, element.syntax.span.clone()));
        }

        let Some(expected_product) = expected_product else {
            return CheckedType::Error;
        };

        let expected_names: HashSet<&str> = expected_product
            .elements
            .iter()
            .filter_map(|field| field.name.as_deref())
            .collect();
        for name in fields.keys() {
            if !expected_names.contains(name.as_str()) {
                self.diagnostics.push(Diagnostic::new(
                    product.syntax.span.clone(),
                    format!("unknown field `{name}` in named product spread"),
                ));
            }
        }

        let mut elements = Vec::with_capacity(expected_product.elements.len());
        let mut has_error = false;
        for expected_element in &expected_product.elements {
            let name = expected_element
                .name
                .clone()
                .expect("checked all-named above");
            match fields.get(&name) {
                Some((value_type, span)) => {
                    match merge_types(value_type.clone(), expected_element.value_type.clone()) {
                        Some(merged) => elements.push(CheckedTypeElement {
                            name: Some(name),
                            value_type: merged,
                        }),
                        None => {
                            has_error = true;
                            self.diagnostics.push(Diagnostic::new(
                                span.clone(),
                                format!(
                                    "expected `{}`, found `{}`",
                                    expected_element.value_type, value_type
                                ),
                            ));
                        }
                    }
                }
                None => {
                    has_error = true;
                    self.diagnostics.push(Diagnostic::new(
                        product.syntax.span.clone(),
                        format!("missing field `{name}` in named product spread"),
                    ));
                }
            }
        }
        if has_error {
            return CheckedType::Error;
        }
        normalize_product_type(elements, false)
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
            Expression::Logical(logical) => self.check_logical_expression(module, logical),
            Expression::Loop(loop_) => self.check_loop_expression(module, loop_, expected),
            Expression::Resource(resource) => {
                let resources = self.resolve_resource_set(
                    module,
                    &crate::ResourceSet {
                        syntax: resource.resource.syntax().clone(),
                        resources: vec![resource.resource.clone()],
                        mutations: Vec::new(),
                    },
                    None,
                );
                let Some(resource_type) = resources.resources.into_iter().next() else {
                    return CheckedType::Error;
                };
                self.resource_types
                    .insert(resource.syntax.id, resource_type.clone());
                resource_type.value_type
            }
            Expression::With(with) => {
                let resources = self.resolve_resource_set(
                    module,
                    &crate::ResourceSet {
                        syntax: with.resource.syntax().clone(),
                        resources: vec![with.resource.clone()],
                        mutations: Vec::new(),
                    },
                    None,
                );
                let Some(resource_type) = resources.resources.into_iter().next() else {
                    return CheckedType::Error;
                };
                self.resource_types
                    .insert(with.syntax.id, resource_type.clone());
                self.check_expression_expected(
                    module,
                    &with.value,
                    Some(&resource_type.value_type),
                );
                self.check_expression_expected(
                    module,
                    &Expression::Block(with.body.clone()),
                    expected,
                )
            }
            Expression::Block(block) => {
                let mut result = CheckedType::empty_product();
                let mut block_returned = false;
                for (index, statement) in block.items.iter().enumerate() {
                    let outer_reachable = self.return_reachable;
                    if block_returned {
                        self.return_reachable = false;
                        self.did_return = false;
                    }
                    if !block_returned
                        && index + 1 == block.items.len()
                        && let Item::Expression(expression) = statement
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
            Expression::Product(product)
                if product.elements.iter().any(|element| element.named_spread) =>
            {
                self.check_named_spread_product(module, product, expected)
            }
            Expression::Product(product) => {
                let mut elements = Vec::new();
                for element in &product.elements {
                    if element.spread {
                        let value_type = self.check_expression(module, &element.value);
                        if self.did_return {
                            return CheckedType::empty_product();
                        }
                        match value_type {
                            CheckedType::Product(product) if !product.variadic => {
                                if elements.len().saturating_add(product.elements.len())
                                    > MAX_PRODUCT_ARITY
                                {
                                    self.diagnostics.push(Diagnostic::new(
                                        element.syntax.span.clone(),
                                        format!(
                                            "product arity exceeds the limit of {MAX_PRODUCT_ARITY}"
                                        ),
                                    ));
                                } else {
                                    elements.extend(product.elements);
                                }
                            }
                            CheckedType::ErasedProduct(_) => {
                                self.diagnostics.push(Diagnostic::new(
                                    element.syntax.span.clone(),
                                    "cannot spread an erased product value",
                                ));
                            }
                            CheckedType::Error => {}
                            other => self.diagnostics.push(Diagnostic::new(
                                element.syntax.span.clone(),
                                format!("cannot spread non-product value of type `{other}`"),
                            )),
                        }
                        continue;
                    }
                    let index = elements.len();
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
                    if elements.len() == MAX_PRODUCT_ARITY {
                        self.diagnostics.push(Diagnostic::new(
                            element.syntax.span.clone(),
                            format!("product arity exceeds the limit of {MAX_PRODUCT_ARITY}"),
                        ));
                    } else {
                        elements.push(CheckedTypeElement {
                            name: element.name.clone(),
                            value_type,
                        });
                    }
                }
                normalize_product_type(elements, false)
            }
            Expression::Call(call) => {
                if let Some(symbol) = module.symbol_for(call.callee.syntax().id)
                    && module.intrinsic_function(symbol) == Some(crate::IntrinsicFunction::SliceFromRef)
                {
                    self.ensure_binding_checked(module, symbol);
                    self.check_expression(module, &call.callee);
                    let argument_type = self.check_expression(module, &call.argument);
                    if self.did_return {
                        return CheckedType::empty_product();
                    }
                    let element = match &argument_type {
                        CheckedType::Ref(payload) => match payload.as_ref() {
                            CheckedType::ErasedProduct(_) => {
                                self.diagnostics.push(Diagnostic::new(
                                    call.argument.syntax().span.clone(),
                                    "`from_ref` requires a fixed-size `Ref`, found a `Slice`",
                                ));
                                None
                            }
                            CheckedType::Product(product) if !product.variadic => {
                                match product.elements.as_slice() {
                                    [] => match expected {
                                        Some(CheckedType::Ref(expected_payload))
                                            if matches!(
                                                expected_payload.as_ref(),
                                                CheckedType::ErasedProduct(_)
                                            ) =>
                                        {
                                            let CheckedType::ErasedProduct(expected_element) =
                                                expected_payload.as_ref()
                                            else {
                                                unreachable!()
                                            };
                                            Some(expected_element.as_ref().clone())
                                        }
                                        _ => {
                                            self.diagnostics.push(Diagnostic::new(
                                                call.syntax.span.clone(),
                                                "cannot infer the element type of an empty slice; annotate the expected type",
                                            ));
                                            None
                                        }
                                    },
                                    [first, rest @ ..] => {
                                        if rest
                                            .iter()
                                            .all(|element| element.value_type == first.value_type)
                                        {
                                            Some(first.value_type.clone())
                                        } else {
                                            self.diagnostics.push(Diagnostic::new(
                                                call.argument.syntax().span.clone(),
                                                "`from_ref` requires a homogeneous array, found mixed element types",
                                            ));
                                            None
                                        }
                                    }
                                }
                            }
                            other => Some(other.clone()),
                        },
                        CheckedType::Error => None,
                        other => {
                            self.diagnostics.push(Diagnostic::new(
                                call.argument.syntax().span.clone(),
                                format!("`from_ref` requires a `Ref` value, found `{other}`"),
                            ));
                            None
                        }
                    };
                    let result = match element {
                        Some(element) => {
                            CheckedType::Ref(Box::new(CheckedType::ErasedProduct(Box::new(element))))
                        }
                        None => CheckedType::Error,
                    };
                    return self.finish_expression_type(expression, result, expected);
                }
                if let Expression::Access(selector) = call.callee.as_ref()
                    && let Accessor::Method(method) = &selector.accessor
                {
                    let receiver_type = self.check_expression(module, &call.argument);
                    if self.did_return {
                        return CheckedType::empty_product();
                    }
                    let receiver_id = self
                        .companion_type_for_expression(module, &call.argument)
                        .or_else(|| checked_type_id(&receiver_type));
                    let Some(receiver_id) = receiver_id else {
                        self.diagnostics.push(Diagnostic::new(
                            selector.syntax.span.clone(),
                            format!("cannot use companion method `{method}` on a value without a named static type"),
                        ));
                        return CheckedType::Error;
                    };
                    let Some(symbol) = module.companion_member(
                        receiver_id,
                        method,
                        self.checking_modules.last().copied(),
                    ) else {
                        self.diagnostics.push(Diagnostic::new(
                            selector.syntax.span.clone(),
                            format!("type has no accessible companion method named `{method}`"),
                        ));
                        return CheckedType::Error;
                    };
                    self.method_symbols.insert(selector.syntax.id, symbol);
                    self.ensure_binding_checked(module, symbol);
                    if let Some(function_id) = self.function_symbols.get(&symbol).copied() {
                        self.ensure_function_checked(module, function_id);
                    }
                    let raw_type = self.symbol_types.get(&symbol).cloned().unwrap_or(CheckedType::Error);
                    let callee_type = self.instantiate_function_use(
                        raw_type.clone(),
                        Some(&receiver_type),
                        expected,
                        selector.syntax.span.clone(),
                    );
                    if let Some(function_id) = self.function_symbols.get(&symbol).copied() {
                        self.check_function_bounds(
                            function_id,
                            &raw_type,
                            &callee_type,
                            selector.syntax.span.clone(),
                        );
                    }
                    self.expression_types.insert(selector.syntax.id, callee_type.clone());
                    let result = match callee_type {
                        CheckedType::Function(function) => {
                            self.check_call_argument(
                                call.argument.syntax().id,
                                receiver_type,
                                &function.parameter,
                                call.argument.syntax().span.clone(),
                            );
                            *function.result
                        }
                        CheckedType::Error => CheckedType::Error,
                        other => {
                            self.diagnostics.push(Diagnostic::new(
                                selector.syntax.span.clone(),
                                format!("companion item `{method}` is not callable: `{other}`"),
                            ));
                            CheckedType::Error
                        }
                    };
                    return self.finish_expression_type(expression, result, expected);
                }
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
                                    call.argument.syntax().id,
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
                    let bare_parameter_string_literal = match (&raw_callee_type, call.argument.as_ref())
                    {
                        (CheckedType::Function(function), Expression::String(string))
                            if matches!(function.parameter.as_ref(), CheckedType::Parameter { .. }) =>
                        {
                            Some(string)
                        }
                        _ => None,
                    };
                    // A bare top-level `T` parameter and a string literal
                    // argument: decode the literal directly into its literal
                    // type, bypassing the shared expected-type-driven
                    // widening pipeline entirely, so `T` narrows to the exact
                    // literal instead of `String`. Scoped to this exact shape
                    // (not routed through `literal_is_admitted`/`merge_types`
                    // generally) so other expected-type-driven checking
                    // elsewhere in the compiler is unaffected.
                    let argument_type = if let Some(string) = bare_parameter_string_literal {
                        match crate::string_literal::decode(&string.literal) {
                            Ok(value) => {
                                let literal_type = CheckedType::StringLiteralSet(vec![value]);
                                self.expression_types
                                    .insert(string.syntax.id, literal_type.clone());
                                literal_type
                            }
                            Err(message) => {
                                self.diagnostics
                                    .push(Diagnostic::new(string.syntax.span.clone(), message));
                                CheckedType::Error
                            }
                        }
                    } else {
                        self.check_expression_expected(module, &call.argument, argument_expected)
                    };
                    if self.did_return {
                        return CheckedType::empty_product();
                    }
                    if let Some(expected_result) = expected
                        && !matches!(expected_result, CheckedType::Sum(_))
                        && !matches!(expected_result,
                            CheckedType::Ref(payload)
                                if matches!(payload.as_ref(), CheckedType::ErasedProduct(_)))
                        && !checked_type_contains_erased_product(&raw_callee_type)
                    {
                        let expected_callee = CheckedType::Function(CheckedFunctionType {
                            parameter: Box::new(widen_literal_type(argument_type.clone())),
                            resources: match &raw_callee_type {
                                CheckedType::Function(function) => function.resources.clone(),
                                _ => CheckedResourceSet::default(),
                            },
                            result: Box::new(expected_result.clone()),
                        });
                        raw_callee_type = self.check_expression_expected(
                            module,
                            &call.callee,
                            Some(&expected_callee),
                        );
                    }
                    let instantiation_expected = expected.filter(|expected| {
                        !matches!(expected,
                            CheckedType::Ref(payload)
                                if matches!(payload.as_ref(), CheckedType::ErasedProduct(_)))
                    });
                    let callee_type = self.instantiate_function_use(
                        raw_callee_type.clone(),
                        Some(&argument_type),
                        instantiation_expected,
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
                                call.argument.syntax().id,
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
                if let CheckedType::Distinct {
                    id, representation, ..
                } = &value_type
                    && let Some(Type::Product(product)) = self
                        .type_declarations
                        .get(id)
                        .and_then(|declaration| declaration.underlying.as_ref())
                    && let [element] = product.elements.as_slice()
                    && matches!(&access.accessor, Accessor::Name(name) if element.name.as_deref() == Some(name))
                {
                    self.accesses.insert(
                        access.syntax.id,
                        CheckedAccess {
                            index: 0,
                            dereference: None,
                            erased: false,
                            scalar: true,
                        },
                    );
                    return representation.as_ref().clone();
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
                            Accessor::Method(_) => None,
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
                                    Accessor::Method(name) => {
                                        format!("unknown companion method `{name}`")
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
                        self.accesses.insert(
                            access.syntax.id,
                            CheckedAccess {
                                index,
                                dereference,
                                erased: false,
                                scalar: false,
                            },
                        );
                        element.value_type.clone()
                    }
                    CheckedType::ErasedProduct(element) => match &access.accessor {
                        Accessor::Index(index) => {
                            let Some(index) = index.parse::<usize>().ok() else {
                                self.diagnostics.push(Diagnostic::new(
                                    access.syntax.span.clone(),
                                    format!("invalid product index `{index}`"),
                                ));
                                return CheckedType::Error;
                            };
                            self.accesses.insert(
                                access.syntax.id,
                                CheckedAccess {
                                    index,
                                    dereference,
                                    erased: true,
                                    scalar: false,
                                },
                            );
                            *element
                        }
                        Accessor::Name(name) => {
                            self.diagnostics.push(Diagnostic::new(
                                access.syntax.span.clone(),
                                format!("erased product has no named element `{name}`"),
                            ));
                            CheckedType::Error
                        }
                        Accessor::Method(name) => {
                            self.diagnostics.push(Diagnostic::new(
                                access.syntax.span.clone(),
                                format!("unknown companion method `{name}`"),
                            ));
                            CheckedType::Error
                        }
                    },
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
            Expression::Index(index) => {
                let target_type = self.check_expression(module, &index.value);
                if self.did_return {
                    return CheckedType::empty_product();
                }
                let mut position_type = self.check_expression(module, &index.index);
                let Some(trait_id) = self.index_trait else {
                    self.diagnostics.push(Diagnostic::new(
                        index.syntax.span.clone(),
                        "standard-library trait `Index` is unavailable",
                    ));
                    return CheckedType::Error;
                };
                let mut partial = vec![
                    target_type.clone(),
                    position_type.clone(),
                    CheckedType::Inferred,
                ];
                let mut resolved = self.resolve_trait_obligation(trait_id, &partial);
                if resolved.is_none()
                    && matches!(index.index.as_ref(), Expression::Integer(_))
                    && position_type != CheckedType::USize
                {
                    position_type = self.check_expression_expected(
                        module,
                        &index.index,
                        Some(&CheckedType::USize),
                    );
                    partial[1] = position_type;
                    resolved = self.resolve_trait_obligation(trait_id, &partial);
                }
                let Some(arguments) = resolved else {
                    self.diagnostics.push(Diagnostic::new(
                        index.syntax.span.clone(),
                        format!("no `Index` implementation is available for `{target_type}`"),
                    ));
                    return CheckedType::Error;
                };
                if structural_index_length(&arguments[0]).is_some()
                    && let Expression::Integer(literal) = index.index.as_ref()
                    && literal
                        .literal
                        .parse::<usize>()
                        .ok()
                        .is_some_and(|position| {
                            structural_index_length(&arguments[0])
                                .is_some_and(|length| position >= length)
                        })
                {
                    self.diagnostics.push(Diagnostic::new(
                        literal.syntax.span.clone(),
                        format!("product index `{}` is out of bounds", literal.literal),
                    ));
                }
                let Some(method) = module
                    .traits()
                    .get(&trait_id)
                    .and_then(|resolved| resolved.methods.first())
                    .copied()
                else {
                    return CheckedType::Error;
                };
                self.trait_dispatches.insert(
                    index.syntax.id,
                    CheckedTraitDispatch {
                        method,
                        arguments: arguments.clone(),
                    },
                );
                arguments[2].clone()
            }
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
            Expression::SyntaxArgument(_)
            | Expression::VisibilityArgument(_)
            | Expression::Quote(_)
            | Expression::Splice(_) => CheckedType::Error,
            Expression::String(string) => {
                let decoded = match crate::string_literal::decode(&string.literal) {
                    Ok(value) => value,
                    Err(message) => {
                        self.diagnostics
                            .push(Diagnostic::new(string.syntax.span.clone(), message));
                        return CheckedType::Error;
                    }
                };
                if expected.is_some_and(|expected| literal_is_admitted(expected, &decoded)) {
                    CheckedType::StringLiteralSet(vec![decoded])
                } else {
                    CheckedType::String
                }
            }
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
            Expression::Float(float) => {
                let float_type = expected
                    .and_then(CheckedType::float_type)
                    .unwrap_or(FloatType::F64);
                let valid = match float_type {
                    FloatType::F32 => float.literal.parse::<f32>().is_ok_and(f32::is_finite),
                    FloatType::F64 => float.literal.parse::<f64>().is_ok_and(f64::is_finite),
                };
                if !valid {
                    self.diagnostics.push(Diagnostic::new(
                        float.syntax.span.clone(),
                        format!(
                            "float literal `{}` does not fit in `{}`",
                            float.literal,
                            float_type.name()
                        ),
                    ));
                }
                CheckedType::float(float_type)
            }
        };
        self.finish_expression_type(expression, natural_type, expected)
    }

    /// `&&`/`||` are not trait-based: both operands and the result are
    /// always `Bool`, resolved from the operator's own synthesized
    /// annotation rather than from user-overloadable trait dispatch. `right`
    /// is checked as its own branch, isolated from `left`'s control flow, so
    /// a `return`/`break` inside `right` never makes the whole expression
    /// look like it unconditionally diverges: the short-circuit path never
    /// evaluates `right` at all.
    fn check_logical_expression(
        &mut self,
        module: &ResolvedModule,
        logical: &crate::LogicalExpression,
    ) -> CheckedType {
        let bool_type = self.resolve_source_type(module, &logical.bool_type);
        self.check_expression_expected(module, &logical.left, Some(&bool_type));
        if self.did_return {
            return CheckedType::empty_product();
        }
        self.check_expression_expected(module, &logical.right, Some(&bool_type));
        self.did_return = false;
        self.logicals.insert(
            logical.syntax.id,
            CheckedLogical {
                bool_type: bool_type.clone(),
            },
        );
        bool_type
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

        let outer_reachable = self.return_reachable;
        let mut previous_patterns = Vec::new();
        let mut values = Vec::new();
        let mut every_arm_returns = true;

        for arm in &match_.arms {
            self.check_match_pattern(module, &arm.pattern, &source);
            if !self.match_pattern_is_useful(module, &source, &previous_patterns, &arm.pattern) {
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

        if self.match_pattern_is_useful(
            module,
            &source,
            &previous_patterns,
            &Pattern::Wildcard(crate::WildcardPattern {
                syntax: match_.syntax.clone(),
                ty: Type::Inferred(crate::InferredType {
                    syntax: match_.syntax.clone(),
                }),
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
                            .filter_map(|row| self.specialize_sum_row(module, row, index, sum))
                            .collect::<Vec<_>>();
                        self.coverage_is_useful(
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

    fn check_loop_expression(
        &mut self,
        module: &ResolvedModule,
        loop_: &crate::LoopExpression,
        expected: Option<&CheckedType>,
    ) -> CheckedType {
        let outer_reachable = self.return_reachable;
        self.loop_contexts.push(LoopCheckContext {
            expected: expected.cloned(),
            breaks: Vec::new(),
        });
        self.did_return = false;
        self.return_reachable = outer_reachable;
        self.check_expression(module, &Expression::Block(loop_.body.clone()));
        let context = self.loop_contexts.pop().expect("loop check context");

        let result = if let Some(expected) = expected {
            expected.clone()
        } else if context.breaks.is_empty() {
            CheckedType::empty_product()
        } else {
            self.join_return_contributions(&context.breaks, loop_.syntax.span.clone())
        };

        if expected.is_none() {
            for contribution in &context.breaks {
                if let Some(syntax) = contribution.syntax {
                    self.coerce_expression_type(
                        syntax,
                        contribution.value_type.clone(),
                        &result,
                        contribution.span.clone(),
                    );
                } else if !can_coerce_type(&contribution.value_type, &result) {
                    self.diagnostics.push(Diagnostic::new(
                        contribution.span.clone(),
                        format!("expected `{result}`, found `{}`", contribution.value_type),
                    ));
                }
            }
        }

        self.did_return = context.breaks.is_empty();
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
            Pattern::At(at) => {
                self.bind_pattern_types(
                    module,
                    &Pattern::Binding(at.binding.as_ref().clone()),
                    value_type,
                );
                self.require_copy_at_pattern(at, value_type);
                self.check_match_pattern(module, &at.pattern, value_type);
            }
            Pattern::Binding(binding) if module.type_for_pattern(binding.syntax.id).is_some() => {
                let expected_id = module
                    .type_for_pattern(binding.syntax.id)
                    .expect("checked singleton pattern");
                let selected = match value_type {
                    CheckedType::Sum(sum) => {
                        let matches = sum
                            .alternatives
                            .iter()
                            .filter(|alternative| {
                                matches!(alternative, CheckedType::Distinct { id, .. } if *id == expected_id)
                            })
                            .collect::<Vec<_>>();
                        (matches.len() == 1).then(|| (*matches[0]).clone())
                    }
                    CheckedType::Distinct { id, .. } if *id == expected_id => {
                        Some(value_type.clone())
                    }
                    _ => None,
                };
                if let Some(selected) = selected {
                    self.pattern_types.insert(binding.syntax.id, selected);
                } else if *value_type != CheckedType::Error {
                    self.diagnostics.push(Diagnostic::new(
                        binding.syntax.span.clone(),
                        format!(
                            "singleton pattern `{}` cannot match `{value_type}`",
                            binding.name
                        ),
                    ));
                }
            }
            Pattern::Binding(binding)
                if !matches!(binding.ty, Type::Inferred(_))
                    && matches!(value_type, CheckedType::Sum(_)) =>
            {
                let declared = self.resolve_source_type(module, &binding.ty);
                let CheckedType::Sum(sum) = value_type else {
                    unreachable!()
                };
                if sum.alternatives.contains(&declared) {
                    self.pattern_types
                        .insert(pattern.syntax().id, declared.clone());
                    self.bind_pattern_types(module, pattern, &declared);
                } else if declared != CheckedType::Error {
                    self.diagnostics.push(Diagnostic::new(
                        binding.syntax.span.clone(),
                        format!(
                            "typed pattern `{declared}` does not select an alternative of `{value_type}`"
                        ),
                    ));
                }
            }
            Pattern::Wildcard(wildcard)
                if !matches!(wildcard.ty, Type::Inferred(_))
                    && matches!(value_type, CheckedType::Sum(_)) =>
            {
                let declared = self.resolve_source_type(module, &wildcard.ty);
                let CheckedType::Sum(sum) = value_type else {
                    unreachable!()
                };
                if sum.alternatives.contains(&declared) {
                    self.pattern_types
                        .insert(pattern.syntax().id, declared.clone());
                    self.bind_pattern_types(module, pattern, &declared);
                } else if declared != CheckedType::Error {
                    self.diagnostics.push(Diagnostic::new(
                        wildcard.syntax.span.clone(),
                        format!(
                            "typed pattern `{declared}` does not select an alternative of `{value_type}`"
                        ),
                    ));
                }
            }
            Pattern::Binding(_) | Pattern::Wildcard(_) => {
                self.bind_pattern_types(module, pattern, value_type);
            }
            Pattern::StringLiteral(pattern) => {
                let Ok(value) = crate::string_literal::decode(&pattern.literal) else {
                    return;
                };
                if *value_type != CheckedType::String && !literal_is_admitted(value_type, &value) {
                    self.diagnostics.push(Diagnostic::new(
                        pattern.syntax.span.clone(),
                        format!(
                            "string pattern `{}` cannot match `{value_type}`",
                            crate::string_literal::encode(&value)
                        ),
                    ));
                }
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
                    CheckedType::String
                        if module.builtin_type(expected_id) == Some(BuiltinType::String) =>
                    {
                        self.string_representation.clone()
                    }
                    CheckedType::Ref(payload)
                        if module.builtin_type(expected_id) == Some(BuiltinType::Ref) =>
                    {
                        if matches!(payload.as_ref(), CheckedType::ErasedProduct(_)) {
                            self.diagnostics.push(Diagnostic::new(
                                pattern.syntax.span.clone(),
                                "an erased product reference cannot be destructured",
                            ));
                            return;
                        }
                        Some(payload.as_ref().clone())
                    }
                    CheckedType::Sum(sum) => {
                        let matches = sum
                            .alternatives
                            .iter()
                            .filter_map(|alternative| match alternative {
                                CheckedType::Distinct {
                                    id, representation, ..
                                } if *id == expected_id => Some(representation.as_ref().clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>();
                        if matches.len() > 1 {
                            self.diagnostics.push(Diagnostic::new(
                                pattern.syntax.span.clone(),
                                format!(
                                    "nominal pattern `{}` selects more than one alternative of `{value_type}`; use a typed pattern to disambiguate",
                                    pattern.name
                                ),
                            ));
                            return;
                        }
                        matches.into_iter().next()
                    }
                    CheckedType::Distinct {
                        id, representation, ..
                    } if *id == expected_id => Some(representation.as_ref().clone()),
                    _ => None,
                };
                if let Some(representation) = representation {
                    self.check_match_pattern(module, &pattern.argument, &representation);
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
            Pattern::Splice(_) => {}
        }
    }

    fn match_pattern_is_useful<'a>(
        &self,
        module: &ResolvedModule,
        value_type: &CheckedType,
        previous: &[&'a Pattern],
        candidate: &'a Pattern,
    ) -> bool {
        let matrix = previous
            .iter()
            .map(|pattern| vec![CoveragePattern::Pattern(pattern)])
            .collect::<Vec<_>>();
        self.coverage_is_useful(
            module,
            std::slice::from_ref(value_type),
            &matrix,
            &[CoveragePattern::Pattern(candidate)],
        )
    }

    fn coverage_is_useful<'a>(
        &self,
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
                let alternatives = match Self::structural_coverage_pattern(candidate[0]) {
                    CoveragePattern::Pattern(Pattern::Binding(binding))
                        if module.type_for_pattern(binding.syntax.id).is_some() =>
                    {
                        let selected_id = module
                            .type_for_pattern(binding.syntax.id)
                            .expect("resolved singleton pattern");
                        sum.alternatives
                            .iter()
                            .position(|alternative| {
                                matches!(alternative, CheckedType::Distinct { id, .. } if *id == selected_id)
                            })
                            .into_iter()
                            .collect()
                    }
                    CoveragePattern::Pattern(Pattern::Binding(binding))
                        if !matches!(binding.ty, Type::Inferred(_)) =>
                    {
                        self.pattern_types
                            .get(&binding.syntax.id)
                            .and_then(|selected| {
                                sum.alternatives
                                    .iter()
                                    .position(|alternative| alternative == selected)
                            })
                            .into_iter()
                            .collect()
                    }
                    _ => match first {
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
                    CoveragePattern::Pattern(Pattern::StringLiteral(pattern)) => {
                        crate::string_literal::decode(&pattern.literal)
                            .ok()
                            .and_then(|value| {
                                sum.alternatives.iter().position(|alternative| {
                                    literal_is_admitted(alternative, &value)
                                })
                            })
                            .into_iter()
                            .collect()
                    }
                    _ => return false,
                    },
                };
                alternatives.into_iter().any(|index| {
                    let representation = match &sum.alternatives[index] {
                        CheckedType::Distinct { representation, .. } => {
                            representation.as_ref().clone()
                        }
                        CheckedType::StringLiteralSet(values) => {
                            CheckedType::StringLiteralSet(values.clone())
                        }
                        alternative => alternative.clone(),
                    };
                    let specialized_matrix = matrix
                        .iter()
                        .filter_map(|row| self.specialize_sum_row(module, row, index, sum))
                        .collect::<Vec<_>>();
                    let Some(specialized_candidate) =
                        self.specialize_sum_row(module, candidate, index, sum)
                    else {
                        return false;
                    };
                    let mut specialized_types = vec![representation];
                    specialized_types.extend_from_slice(&types[1..]);
                    self.coverage_is_useful(
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
                self.coverage_is_useful(
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
                self.coverage_is_useful(
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
                self.coverage_is_useful(
                    module,
                    &specialized_types,
                    &specialized_matrix,
                    &specialized_candidate,
                )
            }
            CheckedType::StringLiteralSet(values) => {
                let candidates = match first {
                    CoveragePattern::Any => values.iter().map(String::as_str).collect::<Vec<_>>(),
                    CoveragePattern::Pattern(Pattern::StringLiteral(pattern)) => {
                        let Ok(value) = crate::string_literal::decode(&pattern.literal) else {
                            return false;
                        };
                        if !values.contains(&value) {
                            return false;
                        }
                        vec![
                            values
                                .iter()
                                .find(|candidate| **candidate == value)
                                .unwrap()
                                .as_str(),
                        ]
                    }
                    _ => return false,
                };
                candidates.into_iter().any(|literal| {
                    let specialized_matrix = matrix
                        .iter()
                        .filter(|row| coverage_pattern_matches_literal(row[0], literal))
                        .map(|row| row[1..].to_vec())
                        .collect::<Vec<_>>();
                    self.coverage_is_useful(
                        module,
                        &types[1..],
                        &specialized_matrix,
                        &candidate[1..],
                    )
                })
            }
            CheckedType::String => match first {
                CoveragePattern::Pattern(Pattern::StringLiteral(pattern)) => {
                    let Ok(literal) = crate::string_literal::decode(&pattern.literal) else {
                        return false;
                    };
                    let specialized_matrix = matrix
                        .iter()
                        .filter(|row| {
                            coverage_pattern_matches_literal(row[0], &literal)
                                || coverage_pattern_is_string_catch_all(module, row[0])
                        })
                        .map(|row| row[1..].to_vec())
                        .collect::<Vec<_>>();
                    self.coverage_is_useful(
                        module,
                        &types[1..],
                        &specialized_matrix,
                        &candidate[1..],
                    )
                }
                CoveragePattern::Any | CoveragePattern::Pattern(Pattern::Nominal(_))
                    if matches!(first, CoveragePattern::Any)
                        || coverage_pattern_is_string_catch_all(module, first) =>
                {
                    let specialized_matrix = matrix
                        .iter()
                        .filter(|row| {
                            matches!(
                                Self::canonical_coverage_pattern(row[0]),
                                CoveragePattern::Any
                            ) || coverage_pattern_is_string_catch_all(module, row[0])
                        })
                        .map(|row| row[1..].to_vec())
                        .collect::<Vec<_>>();
                    self.coverage_is_useful(
                        module,
                        &types[1..],
                        &specialized_matrix,
                        &candidate[1..],
                    )
                }
                _ => false,
            },
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
                self.coverage_is_useful(module, &types[1..], &specialized_matrix, &candidate[1..])
            }
        }
    }

    fn canonical_coverage_pattern(pattern: CoveragePattern<'_>) -> CoveragePattern<'_> {
        match Self::structural_coverage_pattern(pattern) {
            CoveragePattern::Pattern(Pattern::Binding(_) | Pattern::Wildcard(_)) => {
                CoveragePattern::Any
            }
            CoveragePattern::Pattern(Pattern::Product(product)) if product.elements.len() == 1 => {
                Self::canonical_coverage_pattern(CoveragePattern::Pattern(&product.elements[0]))
            }
            other => other,
        }
    }

    fn structural_coverage_pattern(pattern: CoveragePattern<'_>) -> CoveragePattern<'_> {
        match pattern {
            CoveragePattern::Pattern(Pattern::At(at)) => {
                Self::structural_coverage_pattern(CoveragePattern::Pattern(&at.pattern))
            }
            other => other,
        }
    }

    fn specialize_sum_row<'a>(
        &self,
        module: &ResolvedModule,
        row: &[CoveragePattern<'a>],
        index: usize,
        sum: &CheckedSumType,
    ) -> Option<Vec<CoveragePattern<'a>>> {
        let structural = Self::structural_coverage_pattern(row[0]);
        if let CoveragePattern::Pattern(Pattern::Binding(binding)) = structural
            && let Some(selected_id) = module.type_for_pattern(binding.syntax.id)
        {
            if !matches!(&sum.alternatives[index], CheckedType::Distinct { id, .. } if *id == selected_id)
            {
                return None;
            }
            let mut result = vec![CoveragePattern::Any];
            result.extend_from_slice(&row[1..]);
            return Some(result);
        }
        if let CoveragePattern::Pattern(Pattern::Binding(binding)) = structural
            && !matches!(binding.ty, Type::Inferred(_))
        {
            let selected = self.pattern_types.get(&binding.syntax.id)?;
            if selected != &sum.alternatives[index] {
                return None;
            }
            let mut result = vec![CoveragePattern::Any];
            result.extend_from_slice(&row[1..]);
            return Some(result);
        }
        let first = Self::canonical_coverage_pattern(row[0]);
        let selected_id = match &sum.alternatives[index] {
            CheckedType::Distinct { id, .. } => *id,
            CheckedType::String | CheckedType::StringLiteralSet(_) => {
                let head = match first {
                    CoveragePattern::Any => CoveragePattern::Any,
                    CoveragePattern::Pattern(Pattern::StringLiteral(_)) => first,
                    _ => return None,
                };
                let mut result = vec![head];
                result.extend_from_slice(&row[1..]);
                return Some(result);
            }
            _ => {
                if !matches!(first, CoveragePattern::Any) {
                    return None;
                }
                let mut result = vec![CoveragePattern::Any];
                result.extend_from_slice(&row[1..]);
                return Some(result);
            }
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
        let structural = Self::structural_coverage_pattern(row[0]);
        if let CoveragePattern::Pattern(Pattern::Binding(binding)) = structural
            && let Some(selected_id) = module.type_for_pattern(binding.syntax.id)
        {
            if selected_id != id {
                return None;
            }
            let mut result = vec![CoveragePattern::Any];
            result.extend_from_slice(&row[1..]);
            return Some(result);
        }
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
            (actual, CheckedType::Sum(sum)) if !matches!(actual, CheckedType::Sum(_)) => {
                match select_sum_alternative(actual, &sum.alternatives) {
                    Ok(Some(_)) => true,
                    Ok(None) => false,
                    Err(()) => {
                        self.diagnostics.push(Diagnostic::new(
                            span,
                            format!("value of type `{actual}` can be injected into more than one alternative of `{expected}`"),
                        ));
                        return CheckedType::Error;
                    }
                }
            }
            (CheckedType::Sum(actual), CheckedType::Sum(expected)) => {
                actual.alternatives.iter().all(|actual| {
                    matches!(
                        select_sum_alternative(actual, &expected.alternatives),
                        Ok(Some(_))
                    )
                })
            }
            (CheckedType::Ref(actual), CheckedType::Ref(expected))
                if matches!(expected.as_ref(), CheckedType::ErasedProduct(_)) =>
            {
                erased_ref_length(actual, expected).is_some()
            }
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
            let resolved_trait = &module.traits()[&trait_id];
            for parameter in &resolved_trait.parameters {
                substitutions
                    .entry(*parameter)
                    .or_insert(CheckedType::Inferred);
            }
            let partial_arguments = resolved_trait
                .declaration
                .type_parameters
                .iter()
                .map(|pattern| {
                    substitute_type(
                        self.checked_type_parameter_pattern(module, pattern),
                        &substitutions,
                    )
                })
                .collect::<Vec<_>>();
            let Some(arguments) = self.resolve_trait_obligation(trait_id, &partial_arguments)
            else {
                unavailable = true;
                continue;
            };
            if arguments
                .iter()
                .all(|argument| !contains_type_parameter(argument))
                && self
                    .matching_trait_implementations(trait_id, &arguments)
                    .len()
                    > 1
            {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    ambiguous_trait_implementation_failure(&arguments),
                ));
                return CheckedType::Error;
            }
            let completed =
                trait_parameter_values(&self.trait_parameter_arguments[&trait_id], &arguments);
            matches.push((*method, arguments, substitute_type(template, &completed)));
        }
        if matches.len() == 1 {
            let (method, arguments, value_type) = matches.pop().expect("one method match");
            self.trait_dispatches
                .insert(syntax, CheckedTraitDispatch { method, arguments });
            return value_type;
        }
        self.diagnostics.push(Diagnostic::new(
            span,
            if matches.len() > 1 {
                "ambiguous trait method; qualify the trait name"
            } else if unavailable {
                "no trait implementation or matching bound is available"
            } else {
                "could not infer all trait method arguments"
            },
        ));
        CheckedType::Error
    }

    fn trait_obligation_available(&self, trait_id: TraitId, arguments: &[CheckedType]) -> bool {
        self.resolve_trait_obligation(trait_id, arguments).is_some()
    }

    fn resolve_trait_obligation(
        &self,
        trait_id: TraitId,
        arguments: &[CheckedType],
    ) -> Option<Vec<CheckedType>> {
        let bounds = self
            .active_function_bounds
            .iter()
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        if let Some((arguments, _)) = structural_trait_arguments(
            trait_id,
            arguments,
            self.index_trait,
            self.mutate_index_trait,
            self.into_iterator_trait,
            self.iterator_trait,
            |value_type| {
                is_copy_type(
                    value_type,
                    self.copy_trait,
                    self.drop_trait,
                    self.io_type,
                    &self.trait_implementations,
                    &bounds,
                )
            },
        ) {
            return Some(arguments);
        }
        if arguments.iter().any(contains_inferred_type) {
            let parameters = self.trait_parameter_arguments.get(&trait_id)?;
            let query = trait_parameter_values(parameters, arguments);
            let mut matches = self
                .active_function_bounds
                .iter()
                .flatten()
                .filter(|bound| bound.trait_id == trait_id)
                .map(|bound| bound.arguments.as_slice())
                .chain(
                    self.trait_implementations
                        .iter()
                        .filter(|implementation| implementation.trait_id == trait_id)
                        .map(|implementation| implementation.arguments.as_slice()),
                )
                .filter(|candidate| {
                    let candidate = trait_parameter_values(parameters, candidate);
                    query.iter().all(|(parameter, value)| {
                        contains_inferred_type(value)
                            || candidate.get(parameter).is_some_and(|candidate| {
                                !contains_inferred_type(candidate) && candidate == value
                            })
                    })
                });
            let mut completed = matches.next()?.to_vec();
            for candidate in matches {
                completed = merge_trait_arguments(&completed, candidate)?;
            }
            return Some(completed);
        }
        self.trait_obligation_available_exact(trait_id, arguments)
            .then(|| arguments.to_vec())
    }

    fn trait_obligation_available_exact(
        &self,
        trait_id: TraitId,
        arguments: &[CheckedType],
    ) -> bool {
        let bounds = self
            .active_function_bounds
            .iter()
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        if structural_trait_arguments(
            trait_id,
            arguments,
            self.index_trait,
            self.mutate_index_trait,
            self.into_iterator_trait,
            self.iterator_trait,
            |value_type| {
                is_copy_type(
                    value_type,
                    self.copy_trait,
                    self.drop_trait,
                    self.io_type,
                    &self.trait_implementations,
                    &bounds,
                )
            },
        )
        .is_some()
        {
            return true;
        }
        let [target] = arguments else {
            return self.active_function_bounds.iter().rev().any(|bounds| {
                bounds
                    .iter()
                    .any(|bound| bound.trait_id == trait_id && bound.arguments == arguments)
            }) || arguments
                .iter()
                .all(|argument| !contains_type_parameter(argument))
                && !self
                    .matching_trait_implementations(trait_id, arguments)
                    .is_empty();
        };
        if Some(trait_id) == self.sized_trait {
            return target.is_sized()
                || self.active_function_bounds.iter().flatten().any(|bound| {
                    bound.trait_id == trait_id && bound.arguments.as_slice() == arguments
                });
        }
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
                self.io_type,
                &self.trait_implementations,
                &bounds,
            );
        }
        if Some(trait_id) == self.default_trait {
            let bounds = self
                .active_function_bounds
                .iter()
                .flatten()
                .cloned()
                .collect::<Vec<_>>();
            // `is_default_type` only recognizes concrete `Default` impls by
            // exact match; a conditional/blanket `Default` impl is not
            // recognized here. Narrow, disclosed limitation — see the
            // coherence design notes.
            if is_default_type(target, trait_id, &self.trait_implementations, &bounds) {
                return true;
            }
            return !self
                .matching_trait_implementations(trait_id, arguments)
                .is_empty();
        }
        if arguments
            .iter()
            .all(|argument| !contains_type_parameter(argument))
        {
            return !self
                .matching_trait_implementations(trait_id, arguments)
                .is_empty();
        }
        self.active_function_bounds.iter().rev().any(|bounds| {
            bounds
                .iter()
                .any(|bound| bound.trait_id == trait_id && bound.arguments == arguments)
        })
    }

    /// Finds every trait implementation whose (possibly parameterized)
    /// header unifies with `arguments` and whose own bounds hold under that
    /// unification. For a concrete impl this degenerates to structural
    /// equality (unifying a header with no free variables against a
    /// concrete query is equivalent to `==`), so existing concrete impls are
    /// matched exactly as before with no special-casing.
    fn matching_trait_implementations(
        &self,
        trait_id: TraitId,
        arguments: &[CheckedType],
    ) -> Vec<(usize, HashMap<TypeParameterId, CheckedType>)> {
        let key = (trait_id, arguments.to_vec());
        {
            let stack = self.checking_trait_obligations.borrow();
            if stack.len() > 64 || stack.contains(&key) {
                return Vec::new();
            }
        }
        self.checking_trait_obligations.borrow_mut().push(key);
        let matches = self
            .trait_implementations
            .iter()
            .enumerate()
            .filter_map(|(index, implementation)| {
                if implementation.trait_id != trait_id
                    || implementation.arguments.len() != arguments.len()
                {
                    return None;
                }
                let mut substitutions = HashMap::new();
                let unifies = implementation.arguments.iter().zip(arguments).all(
                    |(template, actual)| infer_type_parameters(template, actual, &mut substitutions),
                );
                if !unifies {
                    return None;
                }
                let bounds_hold = implementation.bounds.iter().all(|bound| {
                    let bound_arguments = bound
                        .arguments
                        .iter()
                        .cloned()
                        .map(|argument| substitute_type(argument, &substitutions))
                        .collect::<Vec<_>>();
                    self.trait_obligation_available_exact(bound.trait_id, &bound_arguments)
                });
                bounds_hold.then_some((index, substitutions))
            })
            .collect();
        self.checking_trait_obligations.borrow_mut().pop();
        matches
    }

    fn is_subtype(&self, sub: &CheckedType, sup: &CheckedType) -> bool {
        if matches!(sub, CheckedType::Error | CheckedType::Inferred)
            || matches!(sup, CheckedType::Error | CheckedType::Inferred)
        {
            return true;
        }
        if sub == sup {
            return true;
        }
        if let (CheckedType::Parameter { id: a, .. }, CheckedType::Parameter { id: b, .. }) =
            (sub, sup)
            && a == b
        {
            return true;
        }
        if let CheckedType::Sum(sub_sum) = sub {
            return sub_sum
                .alternatives
                .iter()
                .all(|alternative| self.is_subtype(alternative, sup));
        }
        if let CheckedType::Sum(sup_sum) = sup {
            return sup_sum
                .alternatives
                .iter()
                .any(|alternative| self.is_subtype(sub, alternative));
        }
        if matches!(sub, CheckedType::StringLiteralSet(_)) && *sup == CheckedType::String {
            return true;
        }
        if let (
            CheckedType::StringLiteralSet(sub_values),
            CheckedType::StringLiteralSet(sup_values),
        ) = (sub, sup)
        {
            return sub_values
                .iter()
                .all(|value| sup_values.contains(value));
        }
        if let CheckedType::Parameter { id, .. } = sub {
            return self.active_subtype_bounds.iter().flatten().any(|bound| {
                bound.parameter == *id && self.is_subtype(&bound.supertype, sup)
            });
        }
        false
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
            _ => None,
        }
    }

    fn companion_type_for_expression(
        &self,
        module: &ResolvedModule,
        expression: &Expression,
    ) -> Option<TypeId> {
        match expression {
            Expression::Name(name) => module
                .symbol_for(name.syntax.id)
                .and_then(|symbol| self.symbol_companion_types.get(&symbol).copied()),
            Expression::Call(call) => self
                .function_origin(module, &call.callee)
                .and_then(|function| self.function_result_companion_types.get(&function).copied()),
            Expression::Product(product)
                if product.elements.len() == 1 && !product.elements[0].spread =>
            {
                self.companion_type_for_expression(module, &product.elements[0].value)
            }
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
        let subtype_bounds = self
            .function_subtype_bounds
            .get(&function_id)
            .cloned()
            .unwrap_or_default();
        if bounds.is_empty() && subtype_bounds.is_empty() {
            return;
        }
        let mut substitutions = HashMap::new();
        if !infer_type_parameters(template, instantiated, &mut substitutions) {
            return;
        }
        for bound in bounds {
            let arguments = bound
                .arguments
                .into_iter()
                .map(|argument| substitute_type(argument, &substitutions))
                .collect::<Vec<_>>();
            if !self.trait_obligation_available(bound.trait_id, &arguments) {
                self.diagnostics.push(Diagnostic::new(
                    span.clone(),
                    trait_bound_failure(&arguments),
                ));
            }
        }
        for bound in subtype_bounds {
            let actual = substitutions
                .get(&bound.parameter)
                .cloned()
                .unwrap_or(CheckedType::Error);
            let supertype = substitute_type(bound.supertype, &substitutions);
            if !self.is_subtype(&actual, &supertype) {
                self.diagnostics.push(Diagnostic::new(
                    span.clone(),
                    subtype_bound_failure(&actual, &supertype),
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
        let sized_parameters = sized_type_parameter_ids(&value_type);
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
        for id in &sized_parameters {
            if let Some(replacement) = substitutions.get(id)
                && !replacement.is_sized()
                && *replacement != CheckedType::Error
            {
                self.diagnostics.push(Diagnostic::new(
                    span.clone(),
                    format!(
                        "compile-time argument `{replacement}` does not satisfy the implicit `Sized` bound"
                    ),
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

    fn check_call_argument(
        &mut self,
        syntax: SyntaxId,
        actual: CheckedType,
        expected: &CheckedType,
        span: Span,
    ) {
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
        self.coerce_expression_type(syntax, actual, expected, span);
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
        self.resolve_source_type_inner(module, source_type)
    }

    fn resolve_source_type_inner(
        &mut self,
        module: &ResolvedModule,
        source_type: &Type,
    ) -> CheckedType {
        match source_type {
            Type::Inferred(_) => CheckedType::Inferred,
            Type::StringLiteral(literal) => match crate::string_literal::decode(&literal.literal) {
                Ok(value) => CheckedType::StringLiteralSet(vec![value]),
                Err(message) => {
                    self.diagnostics
                        .push(Diagnostic::new(literal.syntax.span.clone(), message));
                    CheckedType::Error
                }
            },
            Type::Named(named) => {
                if let Some(id) = module.type_parameter_for(named.syntax.id) {
                    CheckedType::Parameter {
                        id,
                        name: named.name.clone(),
                        sized: module.type_parameter_is_sized(id),
                    }
                } else {
                    let resolved = self.resolve_named_type(module, named);
                    self.finish_defaulted_type(module, resolved)
                }
            }
            Type::Product(product) => {
                let span = product.syntax.span.clone();
                let product = self.resolve_product_type(module, product);
                if (product.variadic || product.elements.len() != 1)
                    && product
                        .elements
                        .iter()
                        .any(|element| !element.value_type.is_sized())
                {
                    self.diagnostics
                        .push(Diagnostic::new(span, "product elements must be sized"));
                    return CheckedType::Error;
                }
                normalize_product_type(product.elements, product.variadic)
            }
            Type::Sum(sum) => {
                let alternatives = sum
                    .alternatives
                    .iter()
                    .map(|alternative| self.resolve_source_type_inner(module, alternative))
                    .collect();
                self.normalize_sum_type(alternatives, sum.syntax.span.clone())
            }
            Type::Function(function) => {
                let parameter = self.resolve_source_type_inner(module, &function.parameter);
                let resources = self.resolve_resource_set(
                    module,
                    &function.resources,
                    Some(&function.parameter),
                );
                let result = self.resolve_source_type_inner(module, &function.result);
                if (!parameter.is_sized() && parameter != CheckedType::Error)
                    || (!result.is_sized() && result != CheckedType::Error)
                {
                    self.diagnostics.push(Diagnostic::new(
                        function.syntax.span.clone(),
                        "function parameters and results must be sized",
                    ));
                    return CheckedType::Error;
                }
                CheckedType::Function(CheckedFunctionType {
                    parameter: Box::new(parameter),
                    resources,
                    result: Box::new(result),
                })
            }
            Type::Application(application) => {
                let callee = self.resolve_type_application_callee(module, &application.callee);
                let argument = self.resolve_source_type_inner(module, &application.argument);
                self.apply_type_argument(module, callee, argument, application.syntax.span.clone())
            }
            Type::Repeated(repeated) => {
                let element = self.resolve_source_type_inner(module, &repeated.element);
                if !element.is_sized() && element != CheckedType::Error {
                    self.diagnostics.push(Diagnostic::new(
                        repeated.element.syntax().span.clone(),
                        "product elements must be sized",
                    ));
                    return CheckedType::Error;
                }
                let Some(count) = &repeated.count else {
                    return CheckedType::ErasedProduct(Box::new(element));
                };
                let Ok(count) = count.parse::<usize>() else {
                    self.diagnostics.push(Diagnostic::new(
                        repeated.syntax.span.clone(),
                        "product repetition count is too large",
                    ));
                    return CheckedType::Error;
                };
                if count > MAX_PRODUCT_ARITY {
                    self.diagnostics.push(Diagnostic::new(
                        repeated.syntax.span.clone(),
                        format!("product arity exceeds the limit of {MAX_PRODUCT_ARITY}"),
                    ));
                    return CheckedType::Error;
                }
                normalize_product_type(
                    (0..count)
                        .map(|_| CheckedTypeElement {
                            name: None,
                            value_type: element.clone(),
                        })
                        .collect(),
                    false,
                )
            }
            Type::Splice(_) => CheckedType::Error,
        }
    }

    fn resolve_resource_set(
        &mut self,
        module: &ResolvedModule,
        source: &crate::ResourceSet,
        parameter: Option<&Type>,
    ) -> CheckedResourceSet {
        let mut resources = Vec::new();
        for resource in &source.resources {
            let value_type = self.resolve_source_type_inner(module, resource);
            let valid_nominal = matches!(&value_type, CheckedType::Distinct { .. })
                || matches!(&value_type, CheckedType::Opaque { id, .. } if Some(*id) == self.io_type);
            let concrete = !contains_type_parameter(&value_type)
                && !contains_inferred_type(&value_type)
                && value_type.is_fully_known();
            let copy = is_copy_type(
                &value_type,
                self.copy_trait,
                self.drop_trait,
                self.io_type,
                &self.trait_implementations,
                &[],
            );
            if value_type == CheckedType::Error {
                continue;
            }
            if !valid_nominal || !concrete || !value_type.is_sized() || !copy {
                self.diagnostics.push(Diagnostic::new(
                    resource.syntax().span.clone(),
                    format!(
                        "resource type `{value_type}` must be a concrete, sized, Copy nominal type"
                    ),
                ));
                continue;
            }
            resources.push(CheckedResource { value_type });
        }
        let mut mutations = Vec::new();
        for mutation in &source.mutations {
            if let Some(resolved) = self.resolve_mutation_target(parameter, mutation) {
                mutations.push(resolved);
            }
        }
        CheckedResourceSet::canonical_with_mutations(resources, mutations)
    }

    /// Resolves a `mut`/`mut <index>`/`mut <name>` effect-set entry against
    /// the function's *source* parameter type. Named and positional targets
    /// require a product parameter; resolution uses the source type (not the
    /// checked one) because a single-element product collapses to its bare
    /// element type, dropping the name, once normalized.
    fn resolve_mutation_target(
        &mut self,
        parameter: Option<&Type>,
        mutation: &crate::MutationTarget,
    ) -> Option<CheckedMutation> {
        match &mutation.target {
            crate::MutationTargetKind::Whole => Some(CheckedMutation::Whole),
            crate::MutationTargetKind::Element(index) => {
                let Some(Type::Product(product)) = parameter else {
                    self.diagnostics.push(Diagnostic::new(
                        mutation.syntax.span.clone(),
                        "a positional mutation target requires a product parameter; use `mut` \
                         for the whole parameter",
                    ));
                    return None;
                };
                if *index >= product.elements.len() {
                    self.diagnostics.push(Diagnostic::new(
                        mutation.syntax.span.clone(),
                        format!(
                            "mutation target `{index}` is out of range for a parameter with {} \
                             element(s)",
                            product.elements.len()
                        ),
                    ));
                    return None;
                }
                Some(CheckedMutation::Element(*index))
            }
            crate::MutationTargetKind::Named(name) => {
                let Some(Type::Product(product)) = parameter else {
                    self.diagnostics.push(Diagnostic::new(
                        mutation.syntax.span.clone(),
                        format!(
                            "mutation target `{name}` requires a named product parameter; use \
                             `mut` for the whole parameter"
                        ),
                    ));
                    return None;
                };
                match product
                    .elements
                    .iter()
                    .position(|element| element.name.as_deref() == Some(name.as_str()))
                {
                    Some(index) => Some(CheckedMutation::Element(index)),
                    None => {
                        self.diagnostics.push(Diagnostic::new(
                            mutation.syntax.span.clone(),
                            format!("mutation target `{name}` is not a parameter name"),
                        ));
                        None
                    }
                }
            }
        }
    }

    fn normalize_sum_type(&mut self, alternatives: Vec<CheckedType>, span: Span) -> CheckedType {
        let mut flattened = Vec::new();
        let mut literals = Vec::new();
        let mut contains_string = false;
        let mut pending = alternatives;
        while let Some(alternative) = pending.pop() {
            match alternative {
                CheckedType::Sum(sum) => pending.extend(sum.alternatives),
                CheckedType::StringLiteralSet(values) => literals.extend(values),
                CheckedType::String => contains_string = true,
                other => flattened.push(other),
            }
        }
        flattened.retain(|alternative| *alternative != CheckedType::Error);
        literals.sort();
        literals.dedup();
        if contains_string {
            flattened.push(CheckedType::String);
        } else if !literals.is_empty() {
            flattened.push(CheckedType::StringLiteralSet(literals));
        }
        flattened.sort_by_key(checked_type_sort_key);
        flattened.dedup();
        for alternative in &flattened {
            if !alternative.is_sized() || contains_inferred_type(alternative) {
                self.diagnostics.push(Diagnostic::new(
                    span.clone(),
                    format!("sum alternative `{alternative}` must be a sized type"),
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

    /// Fills unsupplied trailing type parameters of `id` from their default
    /// bounds (`T ?= Default`), given the arguments already supplied as a
    /// prefix. Returns `None` if any unsupplied trailing parameter lacks a
    /// default, is not a plain binding, or is otherwise unresolvable — the
    /// caller should fall back to leaving the type partially applied.
    fn fill_default_type_arguments(
        &mut self,
        module: &ResolvedModule,
        id: TypeId,
        arguments: Vec<CheckedType>,
    ) -> Option<Vec<CheckedType>> {
        let declaration = self.type_declarations[&id].clone();
        if arguments.len() >= declaration.type_parameters.len() {
            return Some(arguments);
        }
        let mut defaults = HashMap::new();
        for bound in &declaration.default_bounds {
            if let Some(checked) = self.resolve_default_bound(module, bound) {
                defaults.insert(checked.parameter, checked.default);
            }
        }
        let mut substitutions = HashMap::new();
        for (pattern, argument) in declaration.type_parameters.iter().zip(&arguments) {
            self.bind_type_argument(module, pattern, argument, &mut substitutions);
        }
        let mut filled = arguments;
        for pattern in &declaration.type_parameters[filled.len()..] {
            let TypeParameterPattern::Binding(binding) = pattern else {
                return None;
            };
            let param_id = module.type_parameter_for(binding.syntax.id)?;
            let default = defaults.get(&param_id)?.clone();
            let resolved = substitute_type(default, &substitutions);
            substitutions.insert(param_id, resolved.clone());
            filled.push(resolved);
        }
        Some(filled)
    }

    /// Resolves the callee position of a type application without filling in
    /// default type arguments: another argument is still coming from the
    /// enclosing application, so a partially applied `TypeConstructor` here
    /// is not yet finished. `Type::Named`/`Type::Application` are the only
    /// shapes a valid callee can take; anything else falls back to the
    /// ordinary resolver, which will report it as not accepting arguments.
    fn resolve_type_application_callee(
        &mut self,
        module: &ResolvedModule,
        callee: &Type,
    ) -> CheckedType {
        match callee {
            Type::Named(named) => {
                if let Some(id) = module.type_parameter_for(named.syntax.id) {
                    CheckedType::Parameter {
                        id,
                        name: named.name.clone(),
                        sized: module.type_parameter_is_sized(id),
                    }
                } else {
                    self.resolve_named_type(module, named)
                }
            }
            Type::Application(application) => {
                let callee = self.resolve_type_application_callee(module, &application.callee);
                let argument = self.resolve_source_type_inner(module, &application.argument);
                self.push_type_argument(module, callee, argument, application.syntax.span.clone())
            }
            other => self.resolve_source_type_inner(module, other),
        }
    }

    /// Pushes one more argument onto a `TypeConstructor` without attempting
    /// to fill in defaults for any parameters still missing afterward. Used
    /// while walking the callee spine of a type application, where more
    /// arguments may still follow.
    fn push_type_argument(
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
        if module.builtin_type(id) == Some(BuiltinType::Ref)
            && matches!(arguments.last(), Some(CheckedType::ErasedProduct(_)))
        {
            self.diagnostics.push(Diagnostic::new(
                span,
                "`Ref` cannot wrap an unsized slice type `T[]`; use `Slice T` instead",
            ));
            return CheckedType::Error;
        }
        self.instantiate_type_declaration(module, id, arguments)
    }

    /// Like `push_type_argument`, but this is the final argument for this
    /// type expression (no enclosing application will supply more), so a
    /// still-partial result is finished off by filling in defaults where
    /// possible.
    fn apply_type_argument(
        &mut self,
        module: &ResolvedModule,
        callee: CheckedType,
        argument: CheckedType,
        span: Span,
    ) -> CheckedType {
        let result = self.push_type_argument(module, callee, argument, span);
        self.finish_defaulted_type(module, result)
    }

    /// Finishes a possibly-partial `TypeConstructor` by filling in defaults
    /// for any trailing parameters still missing, if every one of them has
    /// one. Leaves anything else (including a `TypeConstructor` that still
    /// can't be finished) untouched.
    fn finish_defaulted_type(
        &mut self,
        module: &ResolvedModule,
        value_type: CheckedType,
    ) -> CheckedType {
        let CheckedType::TypeConstructor {
            id,
            name,
            arguments,
        } = value_type
        else {
            return value_type;
        };
        match self.fill_default_type_arguments(module, id, arguments.clone()) {
            Some(filled) => self.instantiate_type_declaration(module, id, filled),
            None => CheckedType::TypeConstructor {
                id,
                name,
                arguments,
            },
        }
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
        for bound in &declaration.trait_bounds {
            let Some(checked_bound) = self.resolve_trait_bound(module, bound) else {
                continue;
            };
            let arguments = checked_bound
                .arguments
                .into_iter()
                .map(|argument| substitute_type(argument, &substitutions))
                .collect::<Vec<_>>();
            if !self.trait_obligation_available(checked_bound.trait_id, &arguments) {
                self.diagnostics.push(Diagnostic::new(
                    bound.syntax.span.clone(),
                    trait_bound_failure(&arguments),
                ));
                return CheckedType::Error;
            }
        }
        for bound in &declaration.subtype_bounds {
            let Some(checked_bound) = self.resolve_subtype_bound(module, bound) else {
                continue;
            };
            let actual = substitutions
                .get(&checked_bound.parameter)
                .cloned()
                .unwrap_or(CheckedType::Error);
            let supertype = substitute_type(checked_bound.supertype, &substitutions);
            if !self.is_subtype(&actual, &supertype) {
                self.diagnostics.push(Diagnostic::new(
                    bound.syntax.span.clone(),
                    subtype_bound_failure(&actual, &supertype),
                ));
                return CheckedType::Error;
            }
        }
        if module.builtin_type(id) == Some(BuiltinType::String) {
            return CheckedType::String;
        }
        if module.builtin_type(id) == Some(BuiltinType::CPointer) {
            return CheckedType::CPointer {
                pointee: Box::new(arguments[0].clone()),
            };
        }
        if module.builtin_type(id) == Some(BuiltinType::Buffer) {
            return CheckedType::Buffer(Box::new(arguments[0].clone()));
        }
        match module.recursive_construction(id) {
            Some(crate::RecursiveConstruction::ManagedReference) => {
                return CheckedType::Ref(Box::new(arguments[0].clone()));
            }
            Some(crate::RecursiveConstruction::Slice) => {
                return CheckedType::Ref(Box::new(CheckedType::ErasedProduct(Box::new(
                    arguments[0].clone(),
                ))));
            }
            Some(crate::RecursiveConstruction::Syntax) => {
                return CheckedType::Opaque {
                    id,
                    name: display_name,
                    arguments,
                };
            }
            None => {}
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
        let mut declaration_bounds = Vec::new();
        for bound in &declaration.trait_bounds {
            if let Some(bound) = self.resolve_trait_bound(module, bound) {
                declaration_bounds.push(bound);
            }
        }
        let declaration_bounds = self.expand_trait_bounds(declaration_bounds);
        if self.bounds_violate_functional_dependencies(&declaration_bounds) {
            self.diagnostics.push(Diagnostic::new(
                declaration.syntax.span.clone(),
                "type declaration bounds conflict with a functional dependency",
            ));
        }
        let mut declaration_subtype_bounds = Vec::new();
        for bound in &declaration.subtype_bounds {
            if let Some(bound) = self.resolve_subtype_bound(module, bound) {
                declaration_subtype_bounds.push(bound);
            }
        }
        self.active_function_bounds.push(declaration_bounds);
        self.active_subtype_bounds.push(declaration_subtype_bounds);
        let template = self.resolve_source_type(
            module,
            declaration.underlying.as_ref().expect("represented type"),
        );
        self.active_function_bounds.pop();
        self.active_subtype_bounds.pop();
        self.resolving_named_types.remove(&id);
        let representation = substitute_type(template, &substitutions);
        if declaration.kind == TypeDeclarationKind::Distinct && !representation.is_sized() {
            self.diagnostics.push(Diagnostic::new(
                declaration.syntax.span.clone(),
                "distinct type representations must be sized",
            ));
            return CheckedType::Error;
        }
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
        if *argument == CheckedType::Inferred {
            match pattern {
                TypeParameterPattern::Binding(binding) => {
                    if let Some(parameter) = module.type_parameter_for(binding.syntax.id) {
                        substitutions.insert(parameter, CheckedType::Inferred);
                    }
                }
                TypeParameterPattern::Product(product) => {
                    for element in &product.elements {
                        self.bind_type_argument(module, element, argument, substitutions);
                    }
                }
                TypeParameterPattern::Splice(_) => {}
            }
            return true;
        }
        match pattern {
            TypeParameterPattern::Binding(binding) => {
                let Some(id) = module.type_parameter_for(binding.syntax.id) else {
                    return false;
                };
                if binding.sized && !argument.is_sized() && *argument != CheckedType::Error {
                    self.diagnostics.push(Diagnostic::new(
                        binding.syntax.span.clone(),
                        format!(
                            "compile-time argument `{argument}` does not satisfy the implicit `Sized` bound for `{}`",
                            binding.name
                        ),
                    ));
                    return false;
                }
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
            TypeParameterPattern::Splice(_) => {
                unreachable!("type-parameter splices must be expanded before type checking")
            }
        }
    }

    fn resolve_product_type(
        &mut self,
        module: &ResolvedModule,
        product: &ProductType,
    ) -> CheckedProductType {
        let mut elements = Vec::new();
        for element in &product.elements {
            if element.spread {
                if let Type::Repeated(repeated) = &element.ty
                    && let Some(count) = &repeated.count
                {
                    let Ok(count) = count.parse::<usize>() else {
                        self.diagnostics.push(Diagnostic::new(
                            repeated.syntax.span.clone(),
                            "product repetition count is too large",
                        ));
                        continue;
                    };
                    let value_type = self.resolve_source_type_inner(module, &repeated.element);
                    if elements.len().saturating_add(count) > MAX_PRODUCT_ARITY {
                        self.diagnostics.push(Diagnostic::new(
                            element.syntax.span.clone(),
                            format!("product arity exceeds the limit of {MAX_PRODUCT_ARITY}"),
                        ));
                        continue;
                    }
                    elements.extend((0..count).map(|_| CheckedTypeElement {
                        name: None,
                        value_type: value_type.clone(),
                    }));
                    continue;
                }
                match self.resolve_source_type_inner(module, &element.ty) {
                    CheckedType::Product(product) if !product.variadic => {
                        if elements.len().saturating_add(product.elements.len()) > MAX_PRODUCT_ARITY
                        {
                            self.diagnostics.push(Diagnostic::new(
                                element.syntax.span.clone(),
                                format!("product arity exceeds the limit of {MAX_PRODUCT_ARITY}"),
                            ));
                        } else {
                            elements.extend(product.elements);
                        }
                    }
                    CheckedType::ErasedProduct(_) => self.diagnostics.push(Diagnostic::new(
                        element.syntax.span.clone(),
                        "cannot spread an erased product",
                    )),
                    CheckedType::Error => {}
                    other => self.diagnostics.push(Diagnostic::new(
                        element.syntax.span.clone(),
                        format!("cannot spread non-product type `{other}`"),
                    )),
                }
            } else if elements.len() == MAX_PRODUCT_ARITY {
                self.diagnostics.push(Diagnostic::new(
                    element.syntax.span.clone(),
                    format!("product arity exceeds the limit of {MAX_PRODUCT_ARITY}"),
                ));
            } else {
                elements.push(CheckedTypeElement {
                    name: element.name.clone(),
                    value_type: self.resolve_source_type_inner(module, &element.ty),
                });
            }
        }
        CheckedProductType {
            elements,
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
                BuiltinType::Float(float) => CheckedType::float(float),
                BuiltinType::String => CheckedType::String,
                BuiltinType::Ref => CheckedType::TypeConstructor {
                    id,
                    name: "Ref".to_owned(),
                    arguments: Vec::new(),
                },
                BuiltinType::Slice => CheckedType::TypeConstructor {
                    id,
                    name: "Slice".to_owned(),
                    arguments: Vec::new(),
                },
                BuiltinType::Buffer => CheckedType::TypeConstructor {
                    id,
                    name: "Buffer".to_owned(),
                    arguments: Vec::new(),
                },
                BuiltinType::CChar => CheckedType::CChar,
                BuiltinType::CString => CheckedType::CString,
                BuiltinType::CPointer => CheckedType::TypeConstructor {
                    id,
                    name: "CPointer".to_owned(),
                    arguments: Vec::new(),
                },
                BuiltinType::IO => CheckedType::Opaque {
                    id,
                    name: "IO".to_owned(),
                    arguments: Vec::new(),
                },
                BuiltinType::Syntax => {
                    let declaration = &self.type_declarations[&id];
                    if declaration.type_parameters.is_empty() {
                        CheckedType::Opaque {
                            id,
                            name: module.type_name(id).unwrap_or(&named.name).to_owned(),
                            arguments: Vec::new(),
                        }
                    } else {
                        CheckedType::TypeConstructor {
                            id,
                            name: module.type_name(id).unwrap_or(&named.name).to_owned(),
                            arguments: Vec::new(),
                        }
                    }
                }
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
        if declaration.kind == TypeDeclarationKind::Distinct && !representation.is_sized() {
            self.diagnostics.push(Diagnostic::new(
                declaration.syntax.span.clone(),
                "distinct type representations must be sized",
            ));
            return CheckedType::Error;
        }
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

pub(crate) fn erased_ref_length(actual: &CheckedType, expected: &CheckedType) -> Option<usize> {
    let CheckedType::ErasedProduct(element) = expected else {
        return None;
    };
    if let CheckedType::ErasedProduct(actual_element) = actual {
        return (actual_element.as_ref() == element.as_ref()).then_some(usize::MAX);
    }
    if actual == element.as_ref() {
        return Some(1);
    }
    let CheckedType::Product(product) = actual else {
        return None;
    };
    if product.elements.is_empty() {
        return Some(0);
    }
    product
        .elements
        .iter()
        .all(|candidate| candidate.value_type == **element)
        .then_some(product.elements.len())
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
        (actual, expected)
            if actual.float_type().is_some() && actual.float_type() == expected.float_type() =>
        {
            Some(actual)
        }
        (CheckedType::String, CheckedType::String) => Some(CheckedType::String),
        (CheckedType::StringLiteralSet(_), CheckedType::String) => Some(CheckedType::String),
        (CheckedType::StringLiteralSet(actual), CheckedType::StringLiteralSet(expected))
            if actual.iter().all(|value| expected.contains(value)) =>
        {
            Some(CheckedType::StringLiteralSet(expected))
        }
        (CheckedType::CString, CheckedType::CString) => Some(CheckedType::CString),
        (CheckedType::CChar, CheckedType::CChar) => Some(CheckedType::CChar),
        (
            CheckedType::Parameter {
                id: actual,
                name,
                sized,
            },
            CheckedType::Parameter { id: expected, .. },
        ) if actual == expected => Some(CheckedType::Parameter {
            id: actual,
            name,
            sized,
        }),
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
        (CheckedType::Buffer(actual), CheckedType::Buffer(expected)) => {
            merge_types(*actual, *expected).map(|value| CheckedType::Buffer(Box::new(value)))
        }
        (CheckedType::ErasedProduct(actual), CheckedType::ErasedProduct(expected)) => {
            merge_types(*actual, *expected).map(|value| CheckedType::ErasedProduct(Box::new(value)))
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
            if actual.resources != expected.resources {
                return None;
            }
            Some(CheckedType::Function(CheckedFunctionType {
                parameter: Box::new(merge_types(*actual.parameter, *expected.parameter)?),
                resources: actual.resources,
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
        (actual, CheckedType::Sum(sum)) if !matches!(actual, CheckedType::Sum(_)) => matches!(
            select_sum_alternative(actual, &sum.alternatives),
            Ok(Some(_))
        ),
        (CheckedType::Sum(actual), CheckedType::Sum(expected)) => {
            actual.alternatives.iter().all(|actual| {
                matches!(
                    select_sum_alternative(actual, &expected.alternatives),
                    Ok(Some(_))
                )
            })
        }
        (CheckedType::Ref(actual), CheckedType::Ref(expected))
            if matches!(expected.as_ref(), CheckedType::ErasedProduct(_)) =>
        {
            erased_ref_length(actual, expected).is_some()
        }
        _ => false,
    }
}

fn literal_is_admitted(value_type: &CheckedType, value: &str) -> bool {
    match value_type {
        CheckedType::String => true,
        CheckedType::StringLiteralSet(values) => values.iter().any(|candidate| candidate == value),
        CheckedType::Sum(sum) => sum
            .alternatives
            .iter()
            .any(|alternative| literal_is_admitted(alternative, value)),
        _ => false,
    }
}

/// Widens `StringLiteralSet` to `String` (recursing into `Sum`
/// alternatives). Used when synthesizing an expected callee type for
/// overload/return-type-driven re-resolution, so a literal-narrowed
/// argument type doesn't leak into that unrelated comparison and fail to
/// merge back against the callee's naturally widened instantiation.
fn widen_literal_type(value_type: CheckedType) -> CheckedType {
    match value_type {
        CheckedType::StringLiteralSet(_) => CheckedType::String,
        CheckedType::Sum(sum) => CheckedType::Sum(CheckedSumType {
            alternatives: sum.alternatives.into_iter().map(widen_literal_type).collect(),
        }),
        other => other,
    }
}

fn coverage_pattern_matches_literal(pattern: CoveragePattern<'_>, literal: &str) -> bool {
    match pattern {
        CoveragePattern::Pattern(Pattern::At(at)) => {
            coverage_pattern_matches_literal(CoveragePattern::Pattern(&at.pattern), literal)
        }
        CoveragePattern::Any
        | CoveragePattern::Pattern(Pattern::Binding(_) | Pattern::Wildcard(_)) => true,
        CoveragePattern::Pattern(Pattern::StringLiteral(pattern)) => {
            crate::string_literal::decode(&pattern.literal)
                .is_ok_and(|candidate| candidate == literal)
        }
        CoveragePattern::Pattern(Pattern::Product(product)) if product.elements.len() == 1 => {
            coverage_pattern_matches_literal(
                CoveragePattern::Pattern(&product.elements[0]),
                literal,
            )
        }
        _ => false,
    }
}

fn coverage_pattern_is_string_catch_all(
    module: &ResolvedModule,
    mut pattern: CoveragePattern<'_>,
) -> bool {
    while let CoveragePattern::Pattern(Pattern::At(at)) = pattern {
        pattern = CoveragePattern::Pattern(&at.pattern);
    }
    let CoveragePattern::Pattern(Pattern::Nominal(pattern)) = pattern else {
        return false;
    };
    module
        .type_for_pattern(pattern.syntax.id)
        .is_some_and(|id| module.builtin_type(id) == Some(BuiltinType::String))
}

pub(crate) fn select_sum_alternative(
    source: &CheckedType,
    alternatives: &[CheckedType],
) -> Result<Option<usize>, ()> {
    if let Some(index) = alternatives
        .iter()
        .position(|alternative| source == alternative)
    {
        return Ok(Some(index));
    }
    let mut matches = alternatives
        .iter()
        .enumerate()
        .filter_map(|(index, alternative)| can_coerce_type(source, alternative).then_some(index));
    let first = matches.next();
    if first.is_some() && matches.next().is_some() {
        Err(())
    } else {
        Ok(first)
    }
}

pub(crate) fn substitute_type(
    value_type: CheckedType,
    substitutions: &HashMap<TypeParameterId, CheckedType>,
) -> CheckedType {
    match value_type {
        CheckedType::Parameter { id, name, sized } => substitutions
            .get(&id)
            .cloned()
            .unwrap_or(CheckedType::Parameter { id, name, sized }),
        CheckedType::CPointer { pointee } => CheckedType::CPointer {
            pointee: Box::new(substitute_type(*pointee, substitutions)),
        },
        CheckedType::Ref(value) => {
            CheckedType::Ref(Box::new(substitute_type(*value, substitutions)))
        }
        CheckedType::Buffer(value) => {
            CheckedType::Buffer(Box::new(substitute_type(*value, substitutions)))
        }
        CheckedType::ErasedProduct(value) => {
            CheckedType::ErasedProduct(Box::new(substitute_type(*value, substitutions)))
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
            resources: CheckedResourceSet::canonical_with_mutations(
                function
                    .resources
                    .resources
                    .into_iter()
                    .map(|resource| CheckedResource {
                        value_type: substitute_type(resource.value_type, substitutions),
                    })
                    .collect(),
                function.resources.mutations,
            ),
            result: Box::new(substitute_type(*function.result, substitutions)),
        }),
        CheckedType::Sum(sum) => normalize_substituted_sum(
            sum.alternatives
                .into_iter()
                .map(|alternative| substitute_type(alternative, substitutions))
                .collect(),
        ),
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

fn normalize_substituted_sum(alternatives: Vec<CheckedType>) -> CheckedType {
    let mut flattened = Vec::new();
    let mut literals = Vec::new();
    let mut contains_string = false;
    let mut pending = alternatives;
    while let Some(alternative) = pending.pop() {
        match alternative {
            CheckedType::Sum(sum) => pending.extend(sum.alternatives),
            CheckedType::StringLiteralSet(values) => literals.extend(values),
            CheckedType::String => contains_string = true,
            CheckedType::Error => {}
            other => flattened.push(other),
        }
    }
    literals.sort();
    literals.dedup();
    if contains_string {
        flattened.push(CheckedType::String);
    } else if !literals.is_empty() {
        flattened.push(CheckedType::StringLiteralSet(literals));
    }
    flattened.sort_by_key(checked_type_sort_key);
    flattened.dedup();
    match flattened.len() {
        0 => CheckedType::Error,
        1 => flattened.pop().expect("one alternative"),
        _ => CheckedType::Sum(CheckedSumType {
            alternatives: flattened,
        }),
    }
}

pub(crate) fn contains_type_parameter(value_type: &CheckedType) -> bool {
    match value_type {
        CheckedType::Parameter { .. } => true,
        CheckedType::CPointer { pointee } => contains_type_parameter(pointee),
        CheckedType::Ref(value) | CheckedType::Buffer(value) => contains_type_parameter(value),
        CheckedType::ErasedProduct(value) => contains_type_parameter(value),
        CheckedType::Opaque { arguments, .. } => arguments.iter().any(contains_type_parameter),
        CheckedType::Product(product) => product
            .elements
            .iter()
            .any(|element| contains_type_parameter(&element.value_type)),
        CheckedType::Function(function) => {
            contains_type_parameter(&function.parameter)
                || function
                    .resources
                    .resources
                    .iter()
                    .any(|resource| contains_type_parameter(&resource.value_type))
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
        CheckedType::Ref(value) | CheckedType::Buffer(value) => contains_inferred_type(value),
        CheckedType::ErasedProduct(value) => contains_inferred_type(value),
        CheckedType::Opaque { arguments, .. } => arguments.iter().any(contains_inferred_type),
        CheckedType::Product(product) => product
            .elements
            .iter()
            .any(|element| contains_inferred_type(&element.value_type)),
        CheckedType::Function(function) => {
            contains_inferred_type(&function.parameter)
                || function
                    .resources
                    .resources
                    .iter()
                    .any(|resource| contains_inferred_type(&resource.value_type))
                || contains_inferred_type(&function.result)
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
            CheckedType::Ref(value) | CheckedType::Buffer(value) => collect(value, ids),
            CheckedType::ErasedProduct(value) => collect(value, ids),
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
                for resource in &function.resources.resources {
                    collect(&resource.value_type, ids);
                }
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

/// Rewrites a trait implementation's own declared type-parameter ids into a
/// stable canonical form, so alpha-equivalent generic impl headers compare
/// equal regardless of the parameter names/ids their authors happened to
/// pick (`impl T => Foo T {..}` and `impl U => Foo U {..}` canonicalize to
/// the same header). This is a syntactic check, not a semantic overlap
/// solver: it does not account for differing bound clauses, so two impls
/// with the same target shape but mutually exclusive bounds are still
/// (conservatively) flagged as duplicates — see the coherence design notes.
fn canonicalize_impl_header(
    parameters: &HashSet<TypeParameterId>,
    arguments: &[CheckedType],
) -> Vec<CheckedType> {
    let mut used = arguments
        .iter()
        .flat_map(type_parameter_ids)
        .filter(|id| parameters.contains(id))
        .collect::<Vec<_>>();
    used.sort_by_key(|id| id.0);
    used.dedup();
    let substitutions = used
        .into_iter()
        .enumerate()
        .map(|(index, id)| {
            (
                id,
                CheckedType::Parameter {
                    id: TypeParameterId(usize::MAX - index),
                    name: format!("#{index}"),
                    sized: true,
                },
            )
        })
        .collect::<HashMap<_, _>>();
    arguments
        .iter()
        .cloned()
        .map(|argument| substitute_type(argument, &substitutions))
        .collect()
}

/// Finds every trait implementation in `implementations` whose (possibly
/// parameterized) header unifies with `arguments`, and whose own bounds hold
/// — checked purely by recursing back into `implementations`, so this does
/// not consider structural traits (`Copy`/`Sized`/`Index`/...) or any
/// generic-body bound context. Used at codegen time (`TypedModule`), after
/// type-checking has already verified every obligation actually reachable
/// from the program's concrete entry points; `TypeChecker` uses its own
/// richer `matching_trait_implementations`, which also consults structural
/// derivation and any active generic-function bounds.
fn dispatch_matching_implementations<'a>(
    implementations: &'a [CheckedTraitImplementation],
    trait_id: TraitId,
    arguments: &[CheckedType],
    seen: &mut Vec<(TraitId, Vec<CheckedType>)>,
) -> Vec<(usize, HashMap<TypeParameterId, CheckedType>)> {
    let key = (trait_id, arguments.to_vec());
    if seen.len() > 64 || seen.contains(&key) {
        return Vec::new();
    }
    seen.push(key);
    let matches = implementations
        .iter()
        .enumerate()
        .filter_map(|(index, implementation)| {
            if implementation.trait_id != trait_id
                || implementation.arguments.len() != arguments.len()
            {
                return None;
            }
            let mut substitutions = HashMap::new();
            let unifies = implementation.arguments.iter().zip(arguments).all(
                |(template, actual)| infer_type_parameters(template, actual, &mut substitutions),
            );
            if !unifies {
                return None;
            }
            let bounds_hold = implementation.bounds.iter().all(|bound| {
                let bound_arguments = bound
                    .arguments
                    .iter()
                    .cloned()
                    .map(|argument| substitute_type(argument, &substitutions))
                    .collect::<Vec<_>>();
                !dispatch_matching_implementations(
                    implementations,
                    bound.trait_id,
                    &bound_arguments,
                    seen,
                )
                .is_empty()
            });
            bounds_hold.then_some((index, substitutions))
        })
        .collect();
    seen.pop();
    matches
}

fn sized_type_parameter_ids(value_type: &CheckedType) -> HashSet<TypeParameterId> {
    fn collect(value_type: &CheckedType, ids: &mut HashSet<TypeParameterId>) {
        match value_type {
            CheckedType::Parameter {
                id, sized: true, ..
            } => {
                ids.insert(*id);
            }
            CheckedType::Parameter { .. } => {}
            CheckedType::CPointer { pointee } => collect(pointee, ids),
            CheckedType::Ref(value) | CheckedType::Buffer(value) | CheckedType::ErasedProduct(value) => collect(value, ids),
            CheckedType::Opaque { arguments, .. }
            | CheckedType::TypeConstructor { arguments, .. } => {
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
            Some(existing) if existing == actual => true,
            Some(existing) => match merge_types(existing.clone(), actual.clone()) {
                Some(merged) => {
                    substitutions.insert(*id, merged);
                    true
                }
                None => false,
            },
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
            let CheckedType::Ref(actual_value) = actual else {
                return false;
            };
            if let CheckedType::ErasedProduct(element) = value.as_ref() {
                return match actual_value.as_ref() {
                    CheckedType::ErasedProduct(actual_element) => {
                        infer_type_parameters(element, actual_element, substitutions)
                    }
                    CheckedType::Product(product) if product.elements.is_empty() => false,
                    CheckedType::Product(product) => {
                        product.homogeneous_element().is_some_and(|actual_element| {
                            infer_type_parameters(element, actual_element, substitutions)
                        })
                    }
                    actual_element => infer_type_parameters(element, actual_element, substitutions),
                };
            }
            infer_type_parameters(value, actual_value, substitutions)
        }
        CheckedType::Buffer(value) => {
            matches!(actual, CheckedType::Buffer(actual_value)
                if infer_type_parameters(value, actual_value, substitutions))
        }
        CheckedType::ErasedProduct(value) => {
            matches!(actual, CheckedType::ErasedProduct(actual_value)
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
            template.resources == actual.resources
                && infer_type_parameters(&template.parameter, &actual.parameter, substitutions)
                && infer_type_parameters(&template.result, &actual.result, substitutions)
        }
        CheckedType::Sum(template) => {
            let CheckedType::Sum(actual) = actual else {
                let normalized = normalize_substituted_sum(
                    template
                        .alternatives
                        .iter()
                        .cloned()
                        .map(|alternative| substitute_type(alternative, substitutions))
                        .collect(),
                );
                return !matches!(normalized, CheckedType::Sum(_))
                    && infer_type_parameters(&normalized, actual, substitutions);
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

fn trait_parameter_values(
    parameters: &[CheckedType],
    arguments: &[CheckedType],
) -> HashMap<TypeParameterId, CheckedType> {
    let mut substitutions = HashMap::new();
    for (parameter, argument) in parameters.iter().zip(arguments) {
        if *argument == CheckedType::Inferred {
            for id in type_parameter_ids(parameter) {
                substitutions.insert(id, CheckedType::Inferred);
            }
        } else {
            infer_type_parameters(parameter, argument, &mut substitutions);
        }
    }
    substitutions
}

fn merge_trait_arguments(left: &[CheckedType], right: &[CheckedType]) -> Option<Vec<CheckedType>> {
    if left.len() != right.len() {
        return None;
    }
    left.iter()
        .cloned()
        .zip(right.iter().cloned())
        .map(|(left, right)| merge_types(left, right))
        .collect()
}

fn clear_function_resources(value_type: &mut CheckedType) {
    match value_type {
        CheckedType::CPointer { pointee }
        | CheckedType::Ref(pointee)
        | CheckedType::Buffer(pointee)
        | CheckedType::ErasedProduct(pointee) => clear_function_resources(pointee),
        CheckedType::Opaque { arguments, .. } | CheckedType::TypeConstructor { arguments, .. } => {
            for argument in arguments {
                clear_function_resources(argument);
            }
        }
        CheckedType::Product(product) => {
            for element in &mut product.elements {
                clear_function_resources(&mut element.value_type);
            }
        }
        CheckedType::Sum(sum) => {
            for alternative in &mut sum.alternatives {
                clear_function_resources(alternative);
            }
        }
        CheckedType::Function(function) => {
            function.resources = CheckedResourceSet::default();
            clear_function_resources(&mut function.parameter);
            clear_function_resources(&mut function.result);
        }
        CheckedType::Distinct {
            arguments,
            representation,
            ..
        } => {
            for argument in arguments {
                clear_function_resources(argument);
            }
            clear_function_resources(representation);
        }
        _ => {}
    }
}

fn infer_type_parameters_for_expected(
    template: &CheckedType,
    expected: &CheckedType,
    substitutions: &mut HashMap<TypeParameterId, CheckedType>,
) -> bool {
    if let (CheckedType::Function(template), CheckedType::Function(expected)) = (template, expected)
    {
        return template.resources == expected.resources
            && infer_type_parameters(&template.parameter, &expected.parameter, substitutions)
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

/// The symbol a single pattern binds at a place-expression root: the name
/// itself for a binding pattern, the alias for an at-pattern (nested
/// bindings are independent copies and are never a place for this position),
/// and nothing for patterns that never bind a writable place.
fn pattern_element_symbol(module: &ResolvedModule, pattern: &Pattern) -> Option<SymbolId> {
    match pattern {
        Pattern::Binding(binding) => module.symbol_for(binding.syntax.id),
        Pattern::At(at) => module.symbol_for(at.binding.syntax.id),
        Pattern::Wildcard(_)
        | Pattern::StringLiteral(_)
        | Pattern::Product(_)
        | Pattern::Nominal(_)
        | Pattern::Splice(_) => None,
    }
}

/// Maps a function's parameter positions to symbols, in the same indexing
/// `CheckedMutation::Element` and source `MutationTargetKind` use: one entry
/// per product element for a parenthesized pattern, otherwise a single
/// "whole parameter" entry at index 0. This mirrors `resolve_mutation_target`
/// resolving against the *source* parameter type rather than the checked one.
fn function_parameter_symbols(module: &ResolvedModule, pattern: &Pattern) -> Vec<Option<SymbolId>> {
    match pattern {
        Pattern::Product(product) => product
            .elements
            .iter()
            .map(|element| pattern_element_symbol(module, element))
            .collect(),
        other => vec![pattern_element_symbol(module, other)],
    }
}

fn pattern_parameter_mutations(pattern: &Pattern) -> Vec<CheckedMutation> {
    let mutations = match pattern {
        Pattern::Binding(binding) if binding.mutable => vec![CheckedMutation::Whole],
        Pattern::At(at) if at.binding.mutable => vec![CheckedMutation::Whole],
        Pattern::Product(product) => product
            .elements
            .iter()
            .enumerate()
            .filter_map(|(index, element)| match element {
                Pattern::Binding(binding) if binding.mutable => {
                    Some(CheckedMutation::Element(index))
                }
                Pattern::At(at) if at.binding.mutable => Some(CheckedMutation::Element(index)),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    CheckedResourceSet::canonical_with_mutations(Vec::new(), mutations).mutations
}

fn mutation_parameter_symbols(
    symbols: &[Option<SymbolId>],
    mutations: &[CheckedMutation],
) -> Vec<SymbolId> {
    if mutations.contains(&CheckedMutation::Whole) {
        return symbols.iter().flatten().copied().collect();
    }
    mutations
        .iter()
        .filter_map(|mutation| match mutation {
            CheckedMutation::Whole => None,
            CheckedMutation::Element(index) => symbols.get(*index).copied().flatten(),
        })
        .collect()
}

/// Walks a place expression to its root symbol, mirroring
/// `TypeChecker::writable_place_issue`'s traversal. Returns the root symbol
/// and whether the walk crossed at least one field/index projection to
/// reach it — a bare name reference is unprojected, `a.b`/`a[i]` are.
fn place_root_symbol(module: &ResolvedModule, expression: &Expression) -> Option<(SymbolId, bool)> {
    fn walk(
        module: &ResolvedModule,
        expression: &Expression,
        projected: bool,
    ) -> Option<(SymbolId, bool)> {
        if let Some(symbol) = module.symbol_for(expression.syntax().id) {
            return Some((symbol, projected));
        }
        match expression {
            Expression::Access(access) => walk(module, &access.value, true),
            Expression::Index(index) => walk(module, &index.value, true),
            _ => None,
        }
    }
    walk(module, expression, false)
}

/// The sub-expression of a call argument that a callee's mutation target
/// refers to: the whole argument for `Whole`, or the matching product
/// element for `Element(i)` when the argument literally is a product
/// expression. When it cannot be structurally decomposed (anything other
/// than a literal product expression), the whole argument stands in,
/// conservatively — there is nothing more precise to point at.
fn call_mutation_target_expression<'a>(
    argument: &'a Expression,
    mutation: &CheckedMutation,
) -> &'a Expression {
    match mutation {
        CheckedMutation::Whole => argument,
        CheckedMutation::Element(index) => match argument {
            Expression::Product(product) => product
                .elements
                .get(*index)
                .map_or(argument, |element| &element.value),
            _ => argument,
        },
    }
}

/// Whether `product` is eligible for a structural derivation (`Index`,
/// `IntoIterator`, `Iterator`): fixed arity, at least one element, and every
/// element `Copy` so that extracting a single element (by index or by
/// iteration) can be done by value without consuming the rest.
fn is_qualifying_product(
    product: &CheckedProductType,
    is_copy: &impl Fn(&CheckedType) -> bool,
) -> bool {
    !product.variadic
        && !product.elements.is_empty()
        && product
            .elements
            .iter()
            .all(|element| is_copy(&element.value_type))
}

/// The duplicate-free sum of `product`'s element types (a single type when
/// `product` is homogeneous). Used both as `Index`'s `Output` and, for the
/// same product, as an iterator's `Item`.
fn product_item(product: &CheckedProductType) -> CheckedType {
    normalize_substituted_sum(
        product
            .elements
            .iter()
            .map(|element| element.value_type.clone())
            .collect(),
    )
}

fn structural_trait_arguments(
    trait_id: TraitId,
    arguments: &[CheckedType],
    index_trait: Option<TraitId>,
    mutate_index_trait: Option<TraitId>,
    into_iterator_trait: Option<TraitId>,
    iterator_trait: Option<TraitId>,
    is_copy: impl Fn(&CheckedType) -> bool,
) -> Option<(Vec<CheckedType>, StructuralTraitMethod)> {
    if Some(trait_id) == into_iterator_trait {
        let [source, iter] = arguments else {
            return None;
        };
        let CheckedType::Product(product) = source else {
            return None;
        };
        if !is_qualifying_product(product, &is_copy) {
            return None;
        }
        let derived_iter = CheckedType::Product(CheckedProductType {
            elements: vec![
                CheckedTypeElement {
                    name: None,
                    value_type: source.clone(),
                },
                CheckedTypeElement {
                    name: None,
                    value_type: CheckedType::USize,
                },
            ],
            variadic: false,
        });
        if *iter == CheckedType::Inferred || *iter == derived_iter {
            return Some((
                vec![source.clone(), derived_iter],
                StructuralTraitMethod::IntoIterator,
            ));
        }
        return None;
    }

    if Some(trait_id) == iterator_trait {
        let [iter, item] = arguments else {
            return None;
        };
        let CheckedType::Product(iter_product) = iter else {
            return None;
        };
        if iter_product.variadic || iter_product.elements.len() != 2 {
            return None;
        }
        if iter_product.elements[1].value_type != CheckedType::USize {
            return None;
        }
        let CheckedType::Product(inner) = &iter_product.elements[0].value_type else {
            return None;
        };
        if !is_qualifying_product(inner, &is_copy) {
            return None;
        }
        let derived_item = product_item(inner);
        if *item == CheckedType::Inferred || *item == derived_item {
            return Some((
                vec![iter.clone(), derived_item],
                StructuralTraitMethod::Iterator,
            ));
        }
        return None;
    }

    let [target, position, dependent] = arguments else {
        return None;
    };
    if *position != CheckedType::USize && *position != CheckedType::Inferred {
        return None;
    }
    let accepts = |actual: &CheckedType| *dependent == CheckedType::Inferred || dependent == actual;

    if Some(trait_id) == index_trait {
        let output = match target {
            CheckedType::Product(product) if is_qualifying_product(product, &is_copy) => {
                product_item(product)
            }
            CheckedType::Ref(payload) => match payload.as_ref() {
                CheckedType::Product(product) if !product.variadic => {
                    let element = product.homogeneous_element()?.clone();
                    is_copy(&element).then_some(element)?
                }
                CheckedType::ErasedProduct(element) => {
                    is_copy(element).then(|| element.as_ref().clone())?
                }
                _ => return None,
            },
            _ => return None,
        };
        if accepts(&output) {
            return Some((
                vec![target.clone(), CheckedType::USize, output],
                StructuralTraitMethod::Index,
            ));
        }
        return None;
    }

    if Some(trait_id) == mutate_index_trait {
        let element = match target {
            CheckedType::Product(product) if !product.variadic => {
                product.homogeneous_element()?.clone()
            }
            CheckedType::Ref(payload) => match payload.as_ref() {
                CheckedType::Product(product) if !product.variadic => {
                    product.homogeneous_element()?.clone()
                }
                CheckedType::ErasedProduct(element) => element.as_ref().clone(),
                _ => return None,
            },
            _ => return None,
        };
        if accepts(&element) {
            return Some((
                vec![target.clone(), CheckedType::USize, element],
                StructuralTraitMethod::MutateIndex,
            ));
        }
    }
    None
}

fn structural_index_length(target: &CheckedType) -> Option<usize> {
    match target {
        CheckedType::Product(product) if !product.variadic && !product.elements.is_empty() => {
            Some(product.elements.len())
        }
        CheckedType::Ref(payload) => match payload.as_ref() {
            CheckedType::Product(product) if !product.variadic && !product.elements.is_empty() => {
                Some(product.elements.len())
            }
            _ => None,
        },
        _ => None,
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
        CheckedType::Ref(value) | CheckedType::Buffer(value) => checked_type_contains_sum(value),
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

fn checked_type_contains_erased_product(value_type: &CheckedType) -> bool {
    match value_type {
        CheckedType::ErasedProduct(_) => true,
        CheckedType::Ref(value) | CheckedType::Buffer(value) | CheckedType::CPointer { pointee: value } => {
            checked_type_contains_erased_product(value)
        }
        CheckedType::Product(product) => product
            .elements
            .iter()
            .any(|element| checked_type_contains_erased_product(&element.value_type)),
        CheckedType::Sum(sum) => sum
            .alternatives
            .iter()
            .any(checked_type_contains_erased_product),
        CheckedType::Function(function) => {
            checked_type_contains_erased_product(&function.parameter)
                || checked_type_contains_erased_product(&function.result)
        }
        CheckedType::Distinct {
            arguments,
            representation,
            ..
        } => {
            arguments.iter().any(checked_type_contains_erased_product)
                || checked_type_contains_erased_product(representation)
        }
        CheckedType::Opaque { arguments, .. } | CheckedType::TypeConstructor { arguments, .. } => {
            arguments.iter().any(checked_type_contains_erased_product)
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
            implementation.trait_id == drop_trait
                && implementation.arguments.len() == 1
                && &implementation.arguments[0] == value_type
        })
    })
}

fn valid_buffer_intrinsic_type(
    value_type: &CheckedType,
    intrinsic: crate::IntrinsicFunction,
) -> bool {
    let CheckedType::Function(function) = value_type else {
        return false;
    };
    let no_effects = CheckedResourceSet::default();
    match intrinsic {
        crate::IntrinsicFunction::BufferWithCapacity => {
            function.parameter.as_ref() == &CheckedType::USize
                && matches!(function.result.as_ref(), CheckedType::Buffer(_))
                && function.resources == no_effects
        }
        crate::IntrinsicFunction::BufferLength | crate::IntrinsicFunction::BufferCapacity => {
            matches!(function.parameter.as_ref(), CheckedType::Buffer(_))
                && function.result.as_ref() == &CheckedType::USize
                && function.resources == no_effects
        }
        crate::IntrinsicFunction::BufferPush => {
            let CheckedType::Product(product) = function.parameter.as_ref() else {
                return false;
            };
            let [buffer, element] = product.elements.as_slice() else {
                return false;
            };
            matches!(&buffer.value_type, CheckedType::Buffer(payload) if payload.as_ref() == &element.value_type)
                && function.result.as_ref() == &CheckedType::empty_product()
                && function.resources.mutations == [CheckedMutation::Element(0)]
                && function.resources.resources.is_empty()
        }
        crate::IntrinsicFunction::BufferPop => {
            let CheckedType::Buffer(element) = function.parameter.as_ref() else {
                return false;
            };
            let CheckedType::Sum(option) = function.result.as_ref() else {
                return false;
            };
            let has_none = option.alternatives.iter().any(|alternative| matches!(
                alternative,
                CheckedType::Distinct { name, representation, .. }
                    if name.ends_with("None") && representation.as_ref() == &CheckedType::empty_product()
            ));
            let has_some = option.alternatives.iter().any(|alternative| matches!(
                alternative,
                CheckedType::Distinct { name, representation, .. }
                    if name.ends_with("Some") && representation.as_ref() == element.as_ref()
            ));
            has_none
                && has_some
                && option.alternatives.len() == 2
                && function.resources.mutations == [CheckedMutation::Whole]
                && function.resources.resources.is_empty()
        }
        crate::IntrinsicFunction::BufferGet => {
            let CheckedType::Product(product) = function.parameter.as_ref() else {
                return false;
            };
            let [buffer, position] = product.elements.as_slice() else {
                return false;
            };
            matches!(
                (&buffer.value_type, function.result.as_ref()),
                (CheckedType::Buffer(element), CheckedType::Ref(result)) if element == result
            ) && position.value_type == CheckedType::USize
                && function.resources == no_effects
        }
        crate::IntrinsicFunction::BufferFreeze => matches!(
            (function.parameter.as_ref(), function.result.as_ref()),
            (CheckedType::Buffer(element), CheckedType::Ref(result))
                if matches!(result.as_ref(), CheckedType::ErasedProduct(result) if result == element)
        ) && function.resources == no_effects,
        crate::IntrinsicFunction::BufferTransfer => {
            let CheckedType::Product(product) = function.parameter.as_ref() else {
                return false;
            };
            let [source, destination] = product.elements.as_slice() else {
                return false;
            };
            matches!(
                (&source.value_type, &destination.value_type),
                (CheckedType::Buffer(a), CheckedType::Buffer(b)) if a == b
            ) && function.result.as_ref() == &CheckedType::empty_product()
                && function.resources.mutations
                    == [CheckedMutation::Element(0), CheckedMutation::Element(1)]
                && function.resources.resources.is_empty()
        }
        _ => false,
    }
}

fn trait_bound_failure(arguments: &[CheckedType]) -> String {
    if let [argument] = arguments {
        format!("trait bound is not satisfied for `{argument}`")
    } else {
        let arguments = arguments
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        format!("trait bound is not satisfied for `({arguments})`")
    }
}

fn subtype_bound_failure(sub: &CheckedType, sup: &CheckedType) -> String {
    format!("subtype bound is not satisfied: `{sub}` is not a subtype of `{sup}`")
}

fn ambiguous_trait_implementation_failure(arguments: &[CheckedType]) -> String {
    if let [argument] = arguments {
        format!("ambiguous trait implementation for `{argument}`; multiple implementations apply")
    } else {
        let arguments = arguments
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "ambiguous trait implementation for `({arguments})`; multiple implementations apply"
        )
    }
}

fn is_copy_type(
    value_type: &CheckedType,
    copy_trait: Option<TraitId>,
    drop_trait: Option<TraitId>,
    io_type: Option<TypeId>,
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
        | CheckedType::F32
        | CheckedType::F64
        | CheckedType::String
        | CheckedType::StringLiteralSet(_)
        | CheckedType::CChar
        | CheckedType::CPointer { .. }
        | CheckedType::Ref(_)
        | CheckedType::Buffer(_)
        | CheckedType::Function(_) => true,
        CheckedType::CString => false,
        CheckedType::Parameter { .. } => copy_trait.is_some_and(|copy_trait| {
            bounds.iter().any(|bound| {
                bound.trait_id == copy_trait
                    && bound.arguments.len() == 1
                    && &bound.arguments[0] == value_type
            })
        }),
        CheckedType::Product(product) => product.elements.iter().all(|element| {
            is_copy_type(
                &element.value_type,
                copy_trait,
                drop_trait,
                io_type,
                implementations,
                bounds,
            )
        }),
        CheckedType::ErasedProduct(_) => false,
        CheckedType::Sum(sum) => sum.alternatives.iter().all(|alternative| {
            is_copy_type(
                alternative,
                copy_trait,
                drop_trait,
                io_type,
                implementations,
                bounds,
            )
        }),
        CheckedType::Distinct { representation, .. } => is_copy_type(
            representation,
            copy_trait,
            drop_trait,
            io_type,
            implementations,
            bounds,
        ),
        CheckedType::Opaque { id, .. } => Some(*id) == io_type,
        CheckedType::TypeConstructor { .. } => false,
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
        CheckedType::Buffer(_) => false,
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

fn is_default_type(
    value_type: &CheckedType,
    default_trait: TraitId,
    implementations: &[CheckedTraitImplementation],
    bounds: &[CheckedTraitBound],
) -> bool {
    if implementations.iter().any(|implementation| {
        implementation.trait_id == default_trait
            && implementation.arguments.len() == 1
            && &implementation.arguments[0] == value_type
    }) {
        return true;
    }
    match value_type {
        CheckedType::Product(product) => product.elements.iter().all(|element| {
            is_default_type(&element.value_type, default_trait, implementations, bounds)
        }),
        CheckedType::Parameter { .. } => bounds.iter().any(|bound| {
            bound.trait_id == default_trait
                && bound.arguments.len() == 1
                && &bound.arguments[0] == value_type
        }),
        _ => false,
    }
}
