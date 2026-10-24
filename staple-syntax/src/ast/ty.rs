use std::fmt;

use super::{SpliceExpression, Syntax};

/// Maximum number of elements a product type or product expression may have.
/// Bounded by the 16-bit tuple index space used throughout the compiler.
pub const MAX_PRODUCT_ARITY: usize = 65_535;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    Inferred(InferredType),
    NumberLiteral(NumberLiteralType),
    StringLiteral(StringLiteralType),
    Named(NamedType),
    Product(ProductType),
    Sum(SumType),
    Function(FunctionType),
    Forall(ForallType),
    Application(TypeApplication),
    EffectApplication(EffectApplication),
    Array(ArrayType),
    Splice(SpliceExpression),
}

impl Type {
    pub fn syntax(&self) -> &Syntax {
        match self {
            Self::Inferred(ty) => &ty.syntax,
            Self::NumberLiteral(ty) => &ty.syntax,
            Self::StringLiteral(ty) => &ty.syntax,
            Self::Named(ty) => &ty.syntax,
            Self::Product(ty) => &ty.syntax,
            Self::Sum(ty) => &ty.syntax,
            Self::Function(ty) => &ty.syntax,
            Self::Forall(ty) => &ty.syntax,
            Self::Application(ty) => &ty.syntax,
            Self::EffectApplication(ty) => &ty.syntax,
            Self::Array(ty) => &ty.syntax,
            Self::Splice(ty) => &ty.syntax,
        }
    }
}

impl fmt::Display for Type {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inferred(_) => formatter.write_str("_"),
            Self::NumberLiteral(literal) => formatter.write_str(&literal.literal),
            Self::StringLiteral(literal) => formatter.write_str(&literal.literal),
            Self::Named(named) => match &named.namespace {
                Some(namespace) => write!(formatter, "{namespace}.{}", named.name),
                None => formatter.write_str(&named.name),
            },
            Self::Product(product) => {
                formatter.write_str("(")?;
                for (index, element) in product.elements.iter().enumerate() {
                    if index > 0 {
                        formatter.write_str(if product.slotted { "; " } else { ", " })?;
                    }
                    if element.spread {
                        formatter.write_str("...")?;
                    } else if let Some(name) = &element.name {
                        write!(formatter, "{name}: ")?;
                    }
                    write!(formatter, "{}", element.ty)?;
                }
                if product.variadic {
                    if !product.elements.is_empty() {
                        formatter.write_str(if product.slotted { "; " } else { ", " })?;
                    }
                    formatter.write_str("...")?;
                }
                if product.slotted && product.elements.len() == 1 {
                    formatter.write_str(";")?;
                }
                formatter.write_str(")")
            }
            Self::Sum(sum) => {
                for (index, alternative) in sum.alternatives.iter().enumerate() {
                    if index > 0 {
                        formatter.write_str(" | ")?;
                    }
                    if matches!(alternative, Type::Forall(_)) {
                        write!(formatter, "({alternative})")?;
                    } else {
                        write!(formatter, "{alternative}")?;
                    }
                }
                Ok(())
            }
            Self::Forall(forall) => write!(formatter, "{} {}", forall.generics, forall.body),
            Self::Function(function) => {
                let parameter = format_mutable_parameter(
                    &function.parameter,
                    &function.mutations(),
                    &function.moves(),
                );
                if function.effects.is_empty() {
                    write!(formatter, "{parameter} -> {}", function.result)
                } else {
                    write!(
                        formatter,
                        "{} ->{} {}",
                        parameter,
                        format_effect_set(&function.effects),
                        function.result
                    )
                }
            }
            Self::Application(outer) => {
                // Source-written `Name<A, B>` renders with angle brackets; an
                // application written by juxtaposition (a trait projection such
                // as `Iterator.Item Iter`) renders as written.
                let bracketed = outer.bracketed;
                let mut arguments = Vec::new();
                let mut head = self;
                while let Self::Application(application) = head {
                    if application.bracketed != bracketed {
                        break;
                    }
                    arguments.push(application.argument.as_ref());
                    head = application.callee.as_ref();
                }
                arguments.reverse();
                if !bracketed {
                    write!(formatter, "{head}")?;
                    for argument in arguments {
                        if matches!(argument, Type::Sum(_) | Type::Function(_) | Type::Forall(_)) {
                            write!(formatter, " ({argument})")?;
                        } else {
                            write!(formatter, " {argument}")?;
                        }
                    }
                    return Ok(());
                }
                write!(formatter, "{head}<")?;
                for (index, argument) in arguments.into_iter().enumerate() {
                    if index > 0 {
                        formatter.write_str(", ")?;
                    }
                    write!(formatter, "{argument}")?;
                }
                formatter.write_str(">")
            }
            Self::EffectApplication(application) => {
                write!(
                    formatter,
                    "{}{}",
                    application.callee,
                    format_effect_set(&application.effects)
                )
            }
            Self::Array(array) => write!(formatter, "({} ^ {})", array.element, array.count),
            Self::Splice(splice) => {
                write!(formatter, "${}", splice.name)?;
                if splice.repeated {
                    formatter.write_str("...")?;
                }
                Ok(())
            }
        }
    }
}

fn format_mutable_parameter(
    parameter: &Type,
    mutations: &[MutationTarget],
    moves: &[MutationTarget],
) -> String {
    let rendered = if matches!(parameter, Type::Function(_) | Type::Forall(_)) {
        format!("({parameter})")
    } else {
        parameter.to_string()
    };
    if mutations
        .iter()
        .any(|mutation| mutation.target == MutationTargetKind::Whole)
    {
        return format!("mut {rendered}");
    }
    if moves
        .iter()
        .any(|target| target.target == MutationTargetKind::Whole)
    {
        return format!("move {rendered}");
    }
    let Type::Product(product) = parameter else {
        return rendered;
    };
    let mut result = String::from("(");
    for (index, element) in product.elements.iter().enumerate() {
        if index > 0 {
            result.push_str(if product.slotted { "; " } else { ", " });
        }
        if mutations
            .iter()
            .any(|mutation| mutation.target == MutationTargetKind::Element(index))
        {
            result.push_str("mut ");
        } else if moves
            .iter()
            .any(|target| target.target == MutationTargetKind::Element(index))
        {
            result.push_str("move ");
        }
        if let Some(name) = &element.name {
            result.push_str(name);
            result.push_str(": ");
        }
        result.push_str(&element.ty.to_string());
    }
    if product.variadic {
        if !product.elements.is_empty() {
            result.push_str(if product.slotted { "; " } else { ", " });
        }
        result.push_str("...");
    }
    if product.slotted && product.elements.len() == 1 {
        result.push(';');
    }
    result.push(')');
    result
}

fn format_effect_set(effects: &EffectSet) -> String {
    let mut entries = Vec::new();
    if let Some(variable) = &effects.variable {
        entries.push(variable.name.clone());
    }
    for state in &effects.state {
        entries.push(state.to_string());
    }
    for resource in &effects.resources {
        entries.push(resource.to_string());
    }
    format!("{{{}}}", entries.join(", "))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringLiteralType {
    pub syntax: Syntax,
    /// The literal exactly as written, including its quotes.
    pub literal: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumberLiteralType {
    pub syntax: Syntax,
    /// The non-negative decimal literal exactly as written.
    pub literal: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SumType {
    pub syntax: Syntax,
    pub alternatives: Vec<Type>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeParameterPattern {
    Binding(TypeParameterBinding),
    Effect(EffectParameterBinding),
    Product(TypeParameterProduct),
    Splice(SpliceExpression),
}

impl TypeParameterPattern {
    pub fn syntax(&self) -> &Syntax {
        match self {
            Self::Binding(binding) => &binding.syntax,
            Self::Effect(binding) => &binding.syntax,
            Self::Product(product) => &product.syntax,
            Self::Splice(splice) => &splice.syntax,
        }
    }

    pub fn names(&self) -> Vec<&str> {
        match self {
            Self::Binding(binding) => vec![binding.name.as_str()],
            Self::Effect(binding) => vec![binding.name.as_str()],
            Self::Product(product) => product
                .elements
                .iter()
                .flat_map(TypeParameterPattern::names)
                .collect(),
            Self::Splice(_) => Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectParameterBinding {
    pub syntax: Syntax,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeParameterBinding {
    pub syntax: Syntax,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeParameterProduct {
    pub syntax: Syntax,
    pub elements: Vec<TypeParameterPattern>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferredType {
    pub syntax: Syntax,
}

impl InferredType {
    pub fn new() -> Self {
        Self {
            syntax: Syntax::compiler(),
        }
    }
}

impl Default for InferredType {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedType {
    pub syntax: Syntax,
    pub namespace: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductType {
    /// Whether top-level elements are slots separated by `;`.
    pub slotted: bool,
    pub syntax: Syntax,
    pub elements: Vec<TypeElement>,
    pub variadic: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeElement {
    pub syntax: Syntax,
    pub name: Option<String>,
    pub ty: Type,
    pub default: Option<Box<super::Expression>>,
    pub spread: bool,
    /// Whether this element of a function parameter is marked `mut`; see
    /// [`FunctionType::mutations`].
    pub mutable: bool,
    /// Whether this element of a function parameter is marked `move`; see
    /// [`FunctionType::moves`].
    pub moved: bool,
}

/// A homogeneous product type, also called an array type: `(element ^ count)`.
///
/// `count` is a compile-time type that must resolve to a non-negative integer
/// singleton or a compile-time parameter bounded by `Natural`. `(T ^ 0)` and
/// `(T ^ 1)` normalize to `()` and `T` respectively.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrayType {
    pub syntax: Syntax,
    pub element: Box<Type>,
    pub count: Box<Type>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionType {
    pub syntax: Syntax,
    pub parameter: Box<Type>,
    /// The `mut` keyword marking the whole parameter, when written. Marks on
    /// individual product elements stay on those elements.
    pub mutable_marker: Option<Syntax>,
    /// The `move` keyword marking the whole parameter, when written.
    pub moved_marker: Option<Syntax>,
    pub effects: EffectSet,
    pub result: Box<Type>,
}

impl FunctionType {
    /// The parameter positions marked `mut`: the whole parameter, or each
    /// marked element of a product parameter.
    pub fn mutations(&self) -> Vec<MutationTarget> {
        marked_targets(self.mutable_marker.as_ref(), &self.parameter, |element| {
            element.mutable
        })
    }

    /// The parameter positions marked `move`, like [`Self::mutations`].
    pub fn moves(&self) -> Vec<MutationTarget> {
        marked_targets(self.moved_marker.as_ref(), &self.parameter, |element| {
            element.moved
        })
    }
}

fn marked_targets(
    whole: Option<&Syntax>,
    parameter: &Type,
    marked: impl Fn(&TypeElement) -> bool,
) -> Vec<MutationTarget> {
    if let Some(syntax) = whole {
        return vec![MutationTarget {
            syntax: syntax.clone(),
            target: MutationTargetKind::Whole,
        }];
    }
    let Type::Product(product) = parameter else {
        return Vec::new();
    };
    product
        .elements
        .iter()
        .enumerate()
        .filter(|(_, element)| marked(element))
        .map(|(index, element)| MutationTarget {
            syntax: element.syntax.clone(),
            target: MutationTargetKind::Element(index),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectSet {
    pub syntax: Syntax,
    pub variable: Option<EffectVariable>,
    pub resources: Vec<ResourceEffect>,
    pub state: Vec<StateEffect>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceEffect {
    pub syntax: Syntax,
    /// A local alias, allowed only in a function value's effect set.
    pub binding: Option<super::NameExpression>,
    pub value_type: Type,
    pub mutable: bool,
}

impl fmt::Display for ResourceEffect {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.mutable {
            formatter.write_str("mut ")?;
        }
        if let Some(binding) = &self.binding {
            write!(formatter, "{}: ", binding.name)?;
        }
        write!(formatter, "{}", self.value_type)
    }
}

impl fmt::Display for EffectSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&format_effect_set(self))
    }
}

impl EffectSet {
    pub fn empty() -> Self {
        Self {
            syntax: Syntax::compiler(),
            variable: None,
            resources: Vec::new(),
            state: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.variable.is_none() && self.resources.is_empty() && self.state.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectVariable {
    pub syntax: Syntax,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateEffect {
    Read,
    Write,
    ReadWrite,
}

impl fmt::Display for StateEffect {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Read => "state.read",
            Self::Write => "state.write",
            Self::ReadWrite => "state",
        })
    }
}

/// A parameter position addressed by a `mut` or `move` marker; reused for
/// both since their target shapes (whole parameter, or one direct element of
/// a top-level product parameter) are identical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationTarget {
    pub syntax: Syntax,
    pub target: MutationTargetKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MutationTargetKind {
    /// `mut`/`move` with no argument: the whole parameter.
    Whole,
    /// A marked element of the top-level product parameter.
    Element(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeApplication {
    pub syntax: Syntax,
    pub callee: Box<Type>,
    pub argument: Box<Type>,
    /// Whether this application was written with angle brackets
    /// (`Name<A, B>`). A bracketed application is a single operand of a
    /// juxtaposed trait argument list, which never splits it.
    pub bracketed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectApplication {
    pub syntax: Syntax,
    pub callee: Box<Type>,
    pub effects: EffectSet,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraitBound {
    pub syntax: Syntax,
    pub trait_name: NamedType,
    pub arguments: Vec<Type>,
}

/// Exact equality of types, at least one of which is an associated projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EqualityBound {
    pub syntax: Syntax,
    pub left: Type,
    pub right: Type,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtypeBound {
    pub syntax: Syntax,
    pub parameter: NamedType,
    pub supertype: Type,
}

/// A `?Sized T` clause, relaxing the implicit `Sized` bound on the
/// compile-time parameter `T` of the same header. Name resolution applies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SizedRelaxation {
    pub syntax: Syntax,
    pub parameter: NamedType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultTypeBound {
    pub syntax: Syntax,
    pub parameter: NamedType,
    pub default: Type,
}

/// A compile-time parameter list and its constraints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenericParameters {
    pub syntax: Syntax,
    pub parameters: Vec<TypeParameterPattern>,
    pub trait_bounds: Vec<TraitBound>,
    pub subtype_bounds: Vec<SubtypeBound>,
    pub equality_bounds: Vec<EqualityBound>,
    pub sized_relaxations: Vec<SizedRelaxation>,
}

/// One declaration parameter, including its optional default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeParameter {
    pub syntax: Syntax,
    pub pattern: TypeParameterPattern,
    pub default: Option<Type>,
}

/// An angle-bracketed declaration parameter list, retaining defaults and bounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeParameters {
    pub syntax: Syntax,
    pub parameters: Vec<TypeParameter>,
    pub generics: GenericParameters,
    /// The `where` clause, without the enclosing angle brackets.
    pub bounds: Syntax,
}

/// A generic function type, `<T where …> body`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForallType {
    pub syntax: Syntax,
    pub generics: GenericParameters,
    pub body: Box<Type>,
}

impl fmt::Display for GenericParameters {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn parameter(p: &TypeParameterPattern) -> String {
            match p {
                TypeParameterPattern::Binding(p) => p.name.clone(),
                TypeParameterPattern::Effect(p) => format!("effect {}", p.name),
                TypeParameterPattern::Product(p) => format!(
                    "({})",
                    p.elements
                        .iter()
                        .map(parameter)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                TypeParameterPattern::Splice(p) => format!("${}...", p.name),
            }
        }
        write!(
            f,
            "<{}",
            self.parameters
                .iter()
                .map(parameter)
                .collect::<Vec<_>>()
                .join(", ")
        )?;
        let mut constraints = self
            .sized_relaxations
            .iter()
            .map(|relaxation| format!("?Sized {}", relaxation.parameter.name))
            .collect::<Vec<_>>();
        constraints.extend(self.trait_bounds.iter().map(|b| {
            let name = Type::Named(b.trait_name.clone()).to_string();
            let arguments = b
                .arguments
                .iter()
                .map(|argument| {
                    if matches!(argument, Type::Function(_) | Type::Forall(_) | Type::Sum(_)) {
                        format!("({argument})")
                    } else {
                        argument.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            format!("{name} {arguments}")
        }));
        constraints.extend(
            self.equality_bounds
                .iter()
                .map(|b| format!("{} == {}", b.left, b.right)),
        );
        constraints.extend(
            self.subtype_bounds
                .iter()
                .map(|b| format!("{} <: {}", b.parameter.name, b.supertype)),
        );
        if !constraints.is_empty() {
            write!(f, " where {}", constraints.join(", "))?;
        }
        f.write_str(">")
    }
}
