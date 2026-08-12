use super::{SpliceExpression, Syntax};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    Inferred(InferredType),
    StringLiteral(StringLiteralType),
    Named(NamedType),
    Product(ProductType),
    Sum(SumType),
    Function(FunctionType),
    Handler(HandlerType),
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
            Self::Handler(ty) => &ty.syntax,
            Self::Application(ty) => &ty.syntax,
            Self::Repeated(ty) => &ty.syntax,
            Self::Splice(ty) => &ty.syntax,
        }
    }
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
}

impl TypeParameterPattern {
    pub fn syntax(&self) -> &Syntax {
        match self {
            Self::Binding(binding) => &binding.syntax,
            Self::Product(product) => &product.syntax,
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
pub struct HandlerType {
    pub syntax: Syntax,
    pub effect: Box<Type>,
    pub effects: EffectSet,
}

/// The concrete, unordered effects attached to one function arrow.
///
/// Source order is retained in the lossless AST. Resolution/type checking
/// canonicalizes the semantic set by effect identity and arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectSet {
    pub syntax: Syntax,
    pub effects: Vec<Type>,
}

impl EffectSet {
    pub fn empty() -> Self {
        Self {
            syntax: Syntax::compiler(),
            effects: Vec::new(),
        }
    }
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
