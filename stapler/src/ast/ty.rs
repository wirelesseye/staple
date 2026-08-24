use std::fmt;

use super::{SpliceExpression, Syntax};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    Inferred(InferredType),
    StringLiteral(StringLiteralType),
    Named(NamedType),
    Product(ProductType),
    Sum(SumType),
    Function(FunctionType),
    Application(TypeApplication),
    Repeated(RepeatedType),
    Splice(SpliceExpression),
}

impl Type {
    pub fn syntax(&self) -> &Syntax {
        match self {
            Self::Inferred(ty) => &ty.syntax,
            Self::StringLiteral(ty) => &ty.syntax,
            Self::Named(ty) => &ty.syntax,
            Self::Product(ty) => &ty.syntax,
            Self::Sum(ty) => &ty.syntax,
            Self::Function(ty) => &ty.syntax,
            Self::Application(ty) => &ty.syntax,
            Self::Repeated(ty) => &ty.syntax,
            Self::Splice(ty) => &ty.syntax,
        }
    }
}

impl fmt::Display for Type {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inferred(_) => formatter.write_str("_"),
            Self::StringLiteral(literal) => formatter.write_str(&literal.literal),
            Self::Named(named) => match &named.namespace {
                Some(namespace) => write!(formatter, "{namespace}.{}", named.name),
                None => formatter.write_str(&named.name),
            },
            Self::Product(product) => {
                formatter.write_str("(")?;
                for (index, element) in product.elements.iter().enumerate() {
                    if index > 0 {
                        formatter.write_str(", ")?;
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
                if function.effects.is_empty() {
                    write!(formatter, "{} -> {}", function.parameter, function.result)
                } else {
                    write!(
                        formatter,
                        "{} ->{} {}",
                        function.parameter,
                        format_effect_set(&function.effects),
                        function.result
                    )
                }
            }
            Self::Application(application) => {
                write!(formatter, "{}", application.callee)?;
                format_type_argument(formatter, &application.argument)
            }
            Self::Repeated(repeated) => match &repeated.count {
                Some(count) => write!(formatter, "{}[{count}]", repeated.element),
                None => write!(formatter, "{}[]", repeated.element),
            },
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

fn format_type_argument(formatter: &mut fmt::Formatter<'_>, argument: &Type) -> fmt::Result {
    if matches!(argument, Type::Sum(_) | Type::Function(_)) {
        write!(formatter, " ({argument})")
    } else {
        write!(formatter, " {argument}")
    }
}

fn format_effect_set(effects: &EffectSet) -> String {
    let mut entries = Vec::new();
    for mutation in &effects.mutations {
        entries.push(match &mutation.target {
            MutationTargetKind::Whole => "mut".to_string(),
            MutationTargetKind::Element(index) => format!("mut {index}"),
            MutationTargetKind::Named(name) => format!("mut {name}"),
        });
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
pub struct SumType {
    pub syntax: Syntax,
    pub alternatives: Vec<Type>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeParameterPattern {
    Binding(TypeParameterBinding),
    Product(TypeParameterProduct),
    Splice(SpliceExpression),
}

impl TypeParameterPattern {
    pub fn syntax(&self) -> &Syntax {
        match self {
            Self::Binding(binding) => &binding.syntax,
            Self::Product(product) => &product.syntax,
            Self::Splice(splice) => &splice.syntax,
        }
    }

    pub fn names(&self) -> Vec<&str> {
        match self {
            Self::Binding(binding) => vec![binding.name.as_str()],
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
pub struct TypeParameterBinding {
    pub syntax: Syntax,
    pub name: String,
    /// Whether this parameter has the language's implicit `Sized` bound.
    pub sized: bool,
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
    pub syntax: Syntax,
    pub elements: Vec<TypeElement>,
    pub variadic: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeElement {
    pub syntax: Syntax,
    pub name: Option<String>,
    pub ty: Type,
    pub spread: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepeatedType {
    pub syntax: Syntax,
    pub element: Box<Type>,
    /// `None` denotes an erased length (`T[]`).
    pub count: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionType {
    pub syntax: Syntax,
    pub parameter: Box<Type>,
    pub effects: EffectSet,
    pub result: Box<Type>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectSet {
    pub syntax: Syntax,
    pub resources: Vec<Type>,
    pub mutations: Vec<MutationTarget>,
    pub state: Vec<StateEffect>,
}

impl EffectSet {
    pub fn empty() -> Self {
        Self {
            syntax: Syntax::compiler(),
            resources: Vec::new(),
            mutations: Vec::new(),
            state: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.resources.is_empty() && self.mutations.is_empty() && self.state.is_empty()
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationTarget {
    pub syntax: Syntax,
    pub target: MutationTargetKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MutationTargetKind {
    /// `mut` with no argument: the whole parameter.
    Whole,
    /// `mut 0`: a positional product element.
    Element(usize),
    /// `mut a`: a named product element.
    Named(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeApplication {
    pub syntax: Syntax,
    pub callee: Box<Type>,
    pub argument: Box<Type>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraitBound {
    pub syntax: Syntax,
    pub trait_name: NamedType,
    pub arguments: Vec<Type>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtypeBound {
    pub syntax: Syntax,
    pub parameter: NamedType,
    pub supertype: Type,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultTypeBound {
    pub syntax: Syntax,
    pub parameter: NamedType,
    pub default: Type,
}
