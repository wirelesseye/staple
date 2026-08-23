use super::{ProductType, SpliceExpression, Syntax, Type, TypeElement};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pattern {
    Binding(BindingPattern),
    At(AtPattern),
    Wildcard(WildcardPattern),
    StringLiteral(StringLiteralPattern),
    Product(ProductPattern),
    Nominal(NominalPattern),
    Splice(SpliceExpression),
}

impl Pattern {
    pub fn syntax(&self) -> &Syntax {
        match self {
            Self::Binding(pattern) => &pattern.syntax,
            Self::At(pattern) => &pattern.syntax,
            Self::Wildcard(pattern) => &pattern.syntax,
            Self::StringLiteral(pattern) => &pattern.syntax,
            Self::Product(pattern) => &pattern.syntax,
            Self::Nominal(pattern) => &pattern.syntax,
            Self::Splice(pattern) => &pattern.syntax,
        }
    }

    pub fn ty(&self) -> Type {
        match self {
            Self::Binding(pattern) => pattern.ty.clone(),
            Self::At(pattern) => {
                if matches!(pattern.binding.ty, Type::Inferred(_)) {
                    pattern.pattern.ty()
                } else {
                    pattern.binding.ty.clone()
                }
            }
            Self::Wildcard(pattern) => pattern.ty.clone(),
            Self::StringLiteral(pattern) => Type::StringLiteral(super::StringLiteralType {
                syntax: pattern.syntax.clone(),
                literal: pattern.literal.clone(),
            }),
            Self::Product(pattern) => {
                let elements = pattern.elements.iter().map(Self::type_element).collect();
                Type::Product(ProductType {
                    syntax: pattern.syntax.clone(),
                    elements,
                    variadic: false,
                })
            }
            Self::Nominal(pattern) => Type::Named(super::NamedType {
                syntax: pattern.syntax.clone(),
                namespace: pattern.namespace.clone(),
                name: pattern.name.clone(),
            }),
            Self::Splice(pattern) => Type::Inferred(super::InferredType {
                syntax: pattern.syntax.clone(),
            }),
        }
    }

    fn type_element(&self) -> TypeElement {
        TypeElement {
            syntax: self.syntax().clone(),
            name: match self {
                Self::Binding(pattern) => Some(pattern.name.clone()),
                Self::At(pattern) => Some(pattern.binding.name.clone()),
                Self::Wildcard(_) => None,
                Self::StringLiteral(_) => None,
                Self::Product(_) => None,
                Self::Nominal(_) => None,
                Self::Splice(_) => None,
            },
            ty: self.ty(),
            spread: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtPattern {
    pub syntax: Syntax,
    pub binding: Box<BindingPattern>,
    pub pattern: Box<Pattern>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringLiteralPattern {
    pub syntax: Syntax,
    /// The literal exactly as written, including its quotes.
    pub literal: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WildcardPattern {
    pub syntax: Syntax,
    pub ty: Type,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingPattern {
    pub syntax: Syntax,
    pub mutable: bool,
    pub name: String,
    /// The spelling before macro hygiene, used to resolve singleton patterns
    /// in the macro definition context.
    pub resolution_name: Option<String>,
    pub ty: Type,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductPattern {
    pub syntax: Syntax,
    pub elements: Vec<Pattern>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NominalPattern {
    pub syntax: Syntax,
    pub namespace: Option<String>,
    pub name: String,
    pub argument: Box<Pattern>,
}
