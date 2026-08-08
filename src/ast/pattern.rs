use super::{ProductType, Syntax, Type, TypeElement};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pattern {
    Binding(BindingPattern),
    Product(ProductPattern),
}

impl Pattern {
    pub fn syntax(&self) -> &Syntax {
        match self {
            Self::Binding(pattern) => &pattern.syntax,
            Self::Product(pattern) => &pattern.syntax,
        }
    }

    pub fn ty(&self) -> Type {
        match self {
            Self::Binding(pattern) => pattern.ty.clone(),
            Self::Product(pattern) => {
                let elements = pattern.elements.iter().map(Self::type_element).collect();
                Type::Product(ProductType {
                    syntax: pattern.syntax.clone(),
                    elements,
                    variadic: false,
                })
            }
        }
    }

    fn type_element(&self) -> TypeElement {
        TypeElement {
            syntax: self.syntax().clone(),
            name: match self {
                Self::Binding(pattern) => Some(pattern.name.clone()),
                Self::Product(_) => None,
            },
            ty: self.ty(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingPattern {
    pub syntax: Syntax,
    pub name: String,
    pub ty: Type,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductPattern {
    pub syntax: Syntax,
    pub elements: Vec<Pattern>,
}
